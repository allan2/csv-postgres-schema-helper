//! Column-type and enum inference.
//!
//! We scan every CSV in the manifest once, accumulating per-column facts:
//! which scalar types every non-null value still satisfies, how many nulls
//! were seen, the set of distinct values (capped, so a high-cardinality
//! column can't blow up memory), and a value [`Profile`] — numeric/temporal
//! range, the digits needed for a `numeric(p, s)`, text length, and
//! character class. From those facts we pick the most specific Postgres type
//! that fits, flag low-cardinality columns as enum-like — exactly the "a
//! loan is fixed or variable" case from the README — and let the report turn
//! the profile into constraint suggestions. Because we scan *all* listed
//! months, a rare value that only appears in a later month is still
//! captured.

use std::collections::{BTreeSet, HashMap};

use crate::{
	error::{Error, Result},
	manifest::Entry,
	util::is_null,
};

/// The Postgres scalar types we infer. The order of the variants is also
/// the specificity order used when several still fit (boolean is most
/// specific, text is the fallback).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColType {
	Boolean,
	Bigint,
	Numeric,
	Date,
	Timestamptz,
	Text,
}

impl ColType {
	/// The literal used both as the column type in DDL and as the cast
	/// target (`$1::<sql>`) when loading text parameters.
	pub const fn sql(self) -> &'static str {
		match self {
			Self::Boolean => "boolean",
			Self::Bigint => "bigint",
			Self::Numeric => "numeric",
			Self::Date => "date",
			Self::Timestamptz => "timestamptz",
			Self::Text => "text",
		}
	}
}

/// One resolved column: its original CSV header, the sanitized SQL
/// identifier, the inferred type, whether it ever held a null, the
/// observed value set when it looks enum-like, and the value profile used
/// to suggest constraints.
pub struct Column {
	pub header: String,
	pub ident: String,
	pub ty: ColType,
	pub nullable: bool,
	pub enum_values: Option<Vec<String>>,
	pub profile: Profile,
}

/// How a column's cardinality reads as a schema-design hint.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cardinality {
	/// Few distinct values (`< 5`): a good enum candidate.
	Enum,
	/// In the `5..=10` grey zone: reported, but no firm call.
	Borderline,
	/// Many distinct values (`> 10`): better as its own lookup table.
	Table,
}

/// Observed facts about a column's *values* (as opposed to its type),
/// gathered across every month and turned into constraint suggestions by
/// the report. `distinct` is exact unless `distinct_overflow` is set, in
/// which case there were strictly more than the enum cap.
pub struct Profile {
	/// Count of non-null values observed across every month.
	pub non_null: usize,
	/// Count of null/blank cells observed across every month.
	pub null_count: usize,
	pub distinct: usize,
	pub distinct_overflow: bool,
	pub enum_cap: usize,
	/// Min/max of values that parsed as numbers (for numeric/bigint cols),
	/// kept as the original text so the range prints exactly.
	pub num_range: Option<(String, String)>,
	/// For a `numeric` column, the tightest `(precision, scale)` that fits
	/// every observed value — i.e. a `numeric(p, s)` suggestion. `None` when
	/// the column is not numeric, has no fractional part, or used scientific
	/// notation (where scale is ambiguous).
	pub num_scale: Option<(usize, usize)>,
	/// Lexical (= chronological for ISO values) min/max, shown for
	/// date/timestamp columns.
	pub temporal_range: Option<(String, String)>,
	/// Min/max character length over non-null values.
	pub len_range: Option<(usize, usize)>,
	pub classes: CharClasses,
}

impl Profile {
	/// Bucket the distinct count into an enum/table recommendation.
	#[must_use]
	pub const fn cardinality(&self) -> Cardinality {
		if self.distinct_overflow || self.distinct > 10 {
			Cardinality::Table
		} else if self.distinct < 5 {
			Cardinality::Enum
		} else {
			Cardinality::Borderline
		}
	}
}

/// Character-class facts over a column's non-null values. Each flag means
/// "every observed value satisfied this"; `has_letter` records whether any
/// letter was seen at all, so "uppercase only" can be distinguished from
/// "no letters present".
#[derive(Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // independent per-class facts, not state
pub struct CharClasses {
	pub any: bool,
	pub all_ascii: bool,
	pub no_lower: bool,
	pub no_upper: bool,
	pub all_alpha: bool,
	pub all_digit: bool,
	pub all_alnum: bool,
	pub has_letter: bool,
}

impl CharClasses {
	const fn new() -> Self {
		Self {
			any: false,
			all_ascii: true,
			no_lower: true,
			no_upper: true,
			all_alpha: true,
			all_digit: true,
			all_alnum: true,
			has_letter: false,
		}
	}

	fn observe(&mut self, s: &str) {
		self.any = true;
		for c in s.chars() {
			self.all_ascii &= c.is_ascii();
			if c.is_alphabetic() {
				self.has_letter = true;
				self.all_digit = false;
				self.no_lower &= !c.is_lowercase();
				self.no_upper &= !c.is_uppercase();
			} else {
				self.all_alpha = false;
				if !c.is_numeric() {
					self.all_digit = false;
					self.all_alnum = false;
				}
			}
		}
	}

	/// A short human label for the tightest character constraint, or `None`
	/// when nothing notable holds.
	#[must_use]
	pub const fn label(self) -> Option<&'static str> {
		if !self.any {
			return None;
		}
		if self.all_digit {
			Some("digits only")
		} else if self.all_alpha && self.has_letter && self.no_lower {
			Some("uppercase letters only")
		} else if self.all_alpha && self.has_letter && self.no_upper {
			Some("lowercase letters only")
		} else if self.all_alpha {
			Some("letters only")
		} else if self.has_letter && self.no_lower {
			Some("uppercase only (no lowercase)")
		} else if self.has_letter && self.no_upper {
			Some("lowercase only (no uppercase)")
		} else if self.all_alnum {
			Some("alphanumeric only")
		} else if self.all_ascii {
			Some("ASCII only")
		} else {
			None
		}
	}
}

/// The whole inferred schema: a Postgres schema name, the account natural
/// key, and one attribute per remaining column. `sources` is kept only to
/// annotate the generated DDL.
pub struct Schema {
	pub name: String,
	pub key: Column,
	pub attrs: Vec<Column>,
	pub sources: Vec<String>,
}

/// Inference knobs from the command line.
pub struct Options {
	pub schema: String,
	pub key: Option<String>,
	pub enum_max: usize,
	pub null_tokens: Vec<String>,
	/// When set (the default), inputs are validated instead of tolerated:
	/// ragged rows, blank account keys, and month-to-month header drift all
	/// abort the run with file context.
	pub strict: bool,
}

/// The candidate scalar types, most specific first. `Accumulator::type_ok`
/// is indexed in this order; [`ColType::Text`] is the fallback and never
/// has a slot (every value satisfies it).
const CANDIDATES: [ColType; 5] = [
	ColType::Boolean,
	ColType::Bigint,
	ColType::Numeric,
	ColType::Date,
	ColType::Timestamptz,
];

/// Apply each candidate's predicate to a value, in [`CANDIDATES`] order.
fn candidate_checks(raw: &str) -> [bool; 5] {
	[
		is_bool(raw),
		is_int(raw),
		is_numeric(raw),
		is_date(raw),
		is_timestamp(raw),
	]
}

/// Per-column accumulator. Each `type_ok` flag starts true and is cleared
/// the first time a non-null value fails the matching predicate.
struct Accumulator {
	non_null: usize,
	saw_null: bool,
	null_count: usize,
	type_ok: [bool; 5],
	distinct: BTreeSet<String>,
	distinct_overflow: bool,
	enum_cap: usize,
	num_min: Option<(f64, String)>,
	num_max: Option<(f64, String)>,
	max_int_digits: usize,
	max_scale: usize,
	scale_known: bool,
	lex_min: Option<String>,
	lex_max: Option<String>,
	len_min: Option<usize>,
	len_max: Option<usize>,
	classes: CharClasses,
}

impl Accumulator {
	const fn new(enum_max: usize) -> Self {
		Self {
			non_null: 0,
			saw_null: false,
			null_count: 0,
			type_ok: [true; 5],
			distinct: BTreeSet::new(),
			distinct_overflow: false,
			enum_cap: enum_max,
			num_min: None,
			num_max: None,
			max_int_digits: 0,
			max_scale: 0,
			scale_known: true,
			lex_min: None,
			lex_max: None,
			len_min: None,
			len_max: None,
			classes: CharClasses::new(),
		}
	}

	fn observe(&mut self, raw: &str, null: bool, enum_max: usize) {
		if null {
			self.saw_null = true;
			self.null_count += 1;
			return;
		}
		self.non_null += 1;
		let checks = candidate_checks(raw);
		for (ok, passed) in self.type_ok.iter_mut().zip(checks) {
			*ok = *ok && passed;
		}
		if !self.distinct_overflow && !self.distinct.contains(raw) {
			if self.distinct.len() >= enum_max {
				self.distinct_overflow = true;
				self.distinct.clear();
			} else {
				self.distinct.insert(raw.to_owned());
			}
		}

		// Numeric range, kept with the original text so it prints exactly,
		// plus the digits needed for a numeric(precision, scale) suggestion.
		if is_numeric(raw)
			&& let Ok(v) = raw.parse::<f64>()
		{
			if self.num_min.as_ref().is_none_or(|(m, _)| v < *m) {
				self.num_min = Some((v, raw.to_owned()));
			}
			if self.num_max.as_ref().is_none_or(|(m, _)| v > *m) {
				self.num_max = Some((v, raw.to_owned()));
			}
			match decimal_parts(raw) {
				Some((int_digits, scale)) => {
					self.max_int_digits = self.max_int_digits.max(int_digits);
					self.max_scale = self.max_scale.max(scale);
				}
				None => self.scale_known = false,
			}
		}

		// Lexical range (chronological for ISO date/timestamp values).
		if self.lex_min.as_deref().is_none_or(|m| raw < m) {
			self.lex_min = Some(raw.to_owned());
		}
		if self.lex_max.as_deref().is_none_or(|m| raw > m) {
			self.lex_max = Some(raw.to_owned());
		}

		// Character length and class facts.
		let len = raw.chars().count();
		self.len_min = Some(self.len_min.map_or(len, |m| m.min(len)));
		self.len_max = Some(self.len_max.map_or(len, |m| m.max(len)));
		self.classes.observe(raw);
	}

	fn profile(&self) -> Profile {
		let ty = self.col_type();
		let num_range = matches!(ty, ColType::Bigint | ColType::Numeric)
			.then(|| {
				self.num_min
					.as_ref()
					.zip(self.num_max.as_ref())
					.map(|((_, lo), (_, hi))| (lo.clone(), hi.clone()))
			})
			.flatten();
		let temporal_range = matches!(ty, ColType::Date | ColType::Timestamptz)
			.then(|| self.lex_min.clone().zip(self.lex_max.clone()))
			.flatten();
		// A numeric(p, s) suggestion only makes sense for a numeric column
		// that actually carries a fractional part and never used exponents.
		let num_scale = (ty == ColType::Numeric && self.scale_known && self.max_scale > 0)
			.then(|| ((self.max_int_digits + self.max_scale).max(1), self.max_scale));
		Profile {
			non_null: self.non_null,
			null_count: self.null_count,
			distinct: if self.distinct_overflow {
				self.enum_cap
			} else {
				self.distinct.len()
			},
			distinct_overflow: self.distinct_overflow,
			enum_cap: self.enum_cap,
			num_range,
			num_scale,
			temporal_range,
			len_range: self.len_min.zip(self.len_max),
			classes: self.classes,
		}
	}

	fn col_type(&self) -> ColType {
		if self.non_null == 0 {
			return ColType::Text;
		}
		self.type_ok
			.iter()
			.zip(CANDIDATES)
			.find_map(|(ok, ty)| ok.then_some(ty))
			.unwrap_or(ColType::Text)
	}

	/// Enum-like means: low cardinality (never overflowed the cap) and at
	/// least one value repeated, so a column of all-distinct ids is not
	/// mistaken for an enum.
	fn enum_values(&self) -> Option<Vec<String>> {
		if self.distinct_overflow {
			return None;
		}
		let distinct = self.distinct.len();
		if distinct == 0 || distinct >= self.non_null {
			return None;
		}
		Some(self.distinct.iter().cloned().collect())
	}
}

/// Scan every CSV in `entries` and resolve a [`Schema`].
///
/// Headers are unioned in first-seen order; the README promises columns
/// don't change month to month, but tolerating an occasional missing
/// column (its rows simply count as null) is cheap and safer than failing
/// the whole run.
pub fn build_schema(entries: &[Entry], opts: &Options) -> Result<Schema> {
	let mut order: Vec<String> = Vec::new();
	let mut accs: HashMap<String, Accumulator> = HashMap::new();
	let mut first_headers: Option<Vec<String>> = None;

	for entry in entries {
		let mut rdr = csv::ReaderBuilder::new()
			.flexible(!opts.strict)
			.from_path(&entry.path)?;
		let headers: Vec<String> = rdr.headers()?.iter().map(ToOwned::to_owned).collect();
		// In strict mode every file must carry the same columns; the README
		// promises they don't change, so drift is almost always a mistake.
		if opts.strict {
			match &first_headers {
				None => first_headers = Some(headers.clone()),
				Some(first) => check_headers(&entry.source, first, &headers)?,
			}
		}
		for h in &headers {
			if !accs.contains_key(h) {
				accs.insert(h.clone(), Accumulator::new(opts.enum_max));
				order.push(h.clone());
			}
		}
		for record in rdr.records() {
			let record = record?;
			for (i, header) in headers.iter().enumerate() {
				let raw = record.get(i).unwrap_or("");
				let null = is_null(raw, &opts.null_tokens);
				if let Some(acc) = accs.get_mut(header) {
					acc.observe(raw, null, opts.enum_max);
				}
			}
		}
	}

	if order.is_empty() {
		return Err(Error::Schema(
			"no columns found in any input CSV".to_owned(),
		));
	}

	let key_header = choose_key(&order, opts.key.as_deref())?;

	let mut used = BTreeSet::new();
	let key = make_column(&key_header, &accs[&key_header], &mut used, false);
	let mut attrs = Vec::with_capacity(order.len() - 1);
	for header in &order {
		if header == &key_header {
			continue;
		}
		attrs.push(make_column(header, &accs[header], &mut used, true));
	}

	Ok(Schema {
		name: sanitize_ident(&opts.schema, &mut BTreeSet::new(), false),
		key,
		attrs,
		sources: entries.iter().map(|e| e.source.clone()).collect(),
	})
}

fn make_column(
	header: &str,
	acc: &Accumulator,
	used: &mut BTreeSet<String>,
	is_attr: bool,
) -> Column {
	Column {
		header: header.to_owned(),
		ident: sanitize_ident(header, used, is_attr),
		ty: acc.col_type(),
		nullable: acc.saw_null || acc.non_null == 0,
		enum_values: if is_attr { acc.enum_values() } else { None },
		profile: acc.profile(),
	}
}

/// Strict-mode header check: the columns of `headers` must match `first`
/// as a set (order may differ). Reports what was added or dropped.
fn check_headers(source: &str, first: &[String], headers: &[String]) -> Result<()> {
	let want: BTreeSet<&String> = first.iter().collect();
	let got: BTreeSet<&String> = headers.iter().collect();
	if want == got {
		return Ok(());
	}
	let missing: Vec<&str> = want.difference(&got).map(|s| s.as_str()).collect();
	let extra: Vec<&str> = got.difference(&want).map(|s| s.as_str()).collect();
	let mut parts = Vec::new();
	if !missing.is_empty() {
		parts.push(format!("missing column(s): {}", missing.join(", ")));
	}
	if !extra.is_empty() {
		parts.push(format!("unexpected column(s): {}", extra.join(", ")));
	}
	Err(Error::Data {
		source: source.to_owned(),
		message: format!("headers differ from the first file ({})", parts.join("; ")),
	})
}

/// Pick the account natural key. An explicit `--key` wins (matched
/// case-insensitively against the headers); otherwise we look for a header
/// that sanitizes to a familiar account-identifier name, falling back to
/// the first column.
fn choose_key(order: &[String], explicit: Option<&str>) -> Result<String> {
	const PREFERRED: [&str; 6] = [
		"account_id",
		"account_number",
		"account",
		"acct_id",
		"id",
		"customer_id",
	];
	if let Some(name) = explicit {
		return order
			.iter()
			.find(|h| h.eq_ignore_ascii_case(name))
			.cloned()
			.ok_or_else(|| Error::Schema(format!("key column `{name}` not found in CSV headers")));
	}
	for want in PREFERRED {
		if let Some(h) = order
			.iter()
			.find(|h| sanitize_ident(h, &mut BTreeSet::new(), false) == want)
		{
			return Ok(h.clone());
		}
	}
	Ok(order[0].clone())
}

/// Turn an arbitrary CSV header into a safe, unique snake-case SQL
/// identifier. Attribute identifiers additionally avoid the structural
/// column names of the temporal tables (`id`, `during`, `request_id`).
fn sanitize_ident(name: &str, used: &mut BTreeSet<String>, is_attr: bool) -> String {
	let mut out = String::new();
	for c in name.chars() {
		if c.is_ascii_alphanumeric() {
			out.push(c.to_ascii_lowercase());
		} else if !out.ends_with('_') {
			out.push('_');
		}
	}
	let trimmed = out.trim_matches('_');
	let mut base = if trimmed.is_empty() {
		"col".to_owned()
	} else if trimmed.as_bytes()[0].is_ascii_digit() {
		format!("c_{trimmed}")
	} else {
		trimmed.to_owned()
	};
	if is_attr && matches!(base.as_str(), "id" | "during" | "request_id") {
		base.push_str("_val");
	}

	let mut candidate = base.clone();
	let mut n = 2u32;
	while used.contains(&candidate) {
		candidate = format!("{base}_{n}");
		n += 1;
	}
	used.insert(candidate.clone());
	candidate
}

fn is_bool(s: &str) -> bool {
	matches!(
		s.to_ascii_lowercase().as_str(),
		"true" | "false" | "t" | "f" | "yes" | "no"
	)
}

fn is_int(s: &str) -> bool {
	s.parse::<i64>().is_ok()
}

/// Plain decimal with optional sign, single dot, and optional exponent.
/// Deliberately rejects `inf`/`nan` (which `f64::parse` would accept) so a
/// stray "inf" can't pin a column to numeric.
fn is_numeric(s: &str) -> bool {
	let b = s.as_bytes();
	let n = b.len();
	let mut i = 0;
	if n == 0 {
		return false;
	}
	if b[i] == b'+' || b[i] == b'-' {
		i += 1;
	}
	let mut digits = 0u32;
	let mut dot = false;
	while i < n && (b[i].is_ascii_digit() || b[i] == b'.') {
		if b[i] == b'.' {
			if dot {
				return false;
			}
			dot = true;
		} else {
			digits += 1;
		}
		i += 1;
	}
	if digits == 0 {
		return false;
	}
	if i < n && (b[i] == b'e' || b[i] == b'E') {
		i += 1;
		if i < n && (b[i] == b'+' || b[i] == b'-') {
			i += 1;
		}
		let mut exp = 0u32;
		while i < n && b[i].is_ascii_digit() {
			exp += 1;
			i += 1;
		}
		if exp == 0 {
			return false;
		}
	}
	i == n
}

/// Split a plain decimal literal into `(significant integer digits,
/// fractional digits)`, e.g. `-420.00` → `(3, 2)` and `0.50` → `(0, 2)`.
/// Returns `None` for scientific notation, where scale is ambiguous. Caller
/// guarantees `s` already passed [`is_numeric`].
fn decimal_parts(s: &str) -> Option<(usize, usize)> {
	if s.bytes().any(|b| b == b'e' || b == b'E') {
		return None;
	}
	let s = s.trim_start_matches(['+', '-']);
	let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
	Some((int_part.trim_start_matches('0').len(), frac_part.len()))
}

/// `YYYY-MM-DD` with plausible month/day ranges. Postgres does the real
/// validation on cast; this just steers inference.
fn is_date(s: &str) -> bool {
	let b = s.as_bytes();
	if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
		return false;
	}
	let digits = |range: std::ops::Range<usize>| range.into_iter().all(|i| b[i].is_ascii_digit());
	if !digits(0..4) || !digits(5..7) || !digits(8..10) {
		return false;
	}
	let month = (b[5] - b'0') * 10 + (b[6] - b'0');
	let day = (b[8] - b'0') * 10 + (b[9] - b'0');
	(1..=12).contains(&month) && (1..=31).contains(&day)
}

/// A date prefix followed by `T`/space and something containing a colon —
/// loose on purpose, so RFC-3339-ish timestamps with or without zone or
/// fractional seconds all qualify.
fn is_timestamp(s: &str) -> bool {
	if s.len() <= 10 {
		return false;
	}
	let (date, rest) = s.split_at(10);
	let sep = rest.as_bytes()[0];
	is_date(date) && (sep == b'T' || sep == b' ') && rest.contains(':')
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeSet;

	use super::{
		Accumulator, Cardinality, ColType, check_headers, decimal_parts, is_date, is_numeric,
		is_timestamp, sanitize_ident,
	};

	fn feed(values: &[&str], enum_max: usize) -> Accumulator {
		let mut acc = Accumulator::new(enum_max);
		for v in values {
			acc.observe(v, v.is_empty(), enum_max);
		}
		acc
	}

	/// Build a column header list from string slices.
	fn headers(names: &[&str]) -> Vec<String> {
		names.iter().map(ToString::to_string).collect()
	}

	#[test]
	fn predicates() {
		assert!(is_numeric("3.14") && is_numeric("-1") && is_numeric("1e3"));
		assert!(!is_numeric("1.2.3") && !is_numeric("inf") && !is_numeric(""));
		assert!(is_date("2024-02-29") && !is_date("2024-13-01") && !is_date("2024-1-1"));
		assert!(is_timestamp("2024-02-01T09:30:00Z") && !is_timestamp("2024-02-01"));
	}

	#[test]
	fn picks_most_specific_type() {
		assert_eq!(feed(&["1", "2", "30"], 32).col_type(), ColType::Bigint);
		assert_eq!(feed(&["1.5", "2", "30"], 32).col_type(), ColType::Numeric);
		assert_eq!(
			feed(&["true", "false", "t"], 32).col_type(),
			ColType::Boolean
		);
		assert_eq!(
			feed(&["2024-01-01", "2024-02-01"], 32).col_type(),
			ColType::Date
		);
		assert_eq!(feed(&["fixed", "variable"], 32).col_type(), ColType::Text);
	}

	#[test]
	fn enum_detection() {
		// Repeated low-cardinality values are enum-like.
		let acc = feed(&["a", "b", "a", "b", "a"], 32);
		assert_eq!(
			acc.enum_values(),
			Some(vec!["a".to_owned(), "b".to_owned()])
		);
		// All-distinct (ids) are not.
		assert_eq!(feed(&["1", "2", "3"], 32).enum_values(), None);
		// Past the cap, not enum-like.
		assert_eq!(feed(&["a", "b", "c"], 2).enum_values(), None);
	}

	#[test]
	fn nullable_tracks_empties() {
		let acc = feed(&["x", "", "y"], 32);
		assert!(acc.saw_null);
		assert_eq!(acc.non_null, 2);
	}

	#[test]
	fn profiles_numeric_range_and_scale() {
		let p = feed(&["1500.00", "-420.00", "30000.00"], 32).profile();
		assert_eq!(p.num_range, Some(("-420.00".to_owned(), "30000.00".to_owned())));
		// 30000 has 5 integer digits, scale 2 -> numeric(7, 2).
		assert_eq!(p.num_scale, Some((7, 2)));
		// Integers carry no scale suggestion (they're bigint anyway).
		assert_eq!(feed(&["1", "2", "30"], 32).profile().num_scale, None);
		// Scientific notation makes scale ambiguous.
		assert_eq!(feed(&["1.5", "2e3"], 32).profile().num_scale, None);
	}

	#[test]
	fn decimal_parts_splits_digits() {
		assert_eq!(decimal_parts("-420.00"), Some((3, 2)));
		assert_eq!(decimal_parts("0.50"), Some((0, 2)));
		assert_eq!(decimal_parts("1500"), Some((4, 0)));
		assert_eq!(decimal_parts("1e3"), None);
	}

	#[test]
	fn cardinality_buckets() {
		let mk = |n: usize| {
			let vals: Vec<String> = (0..n).map(|i| format!("v{i}")).collect();
			let refs: Vec<&str> = vals.iter().map(String::as_str).collect();
			feed(&refs, 64).profile().cardinality()
		};
		assert_eq!(mk(3), Cardinality::Enum); // < 5
		assert_eq!(mk(7), Cardinality::Borderline); // 5..=10
		assert_eq!(mk(20), Cardinality::Table); // > 10
		// Past the enum cap, definitely a table.
		let p = feed(&["a", "b", "c"], 2).profile();
		assert!(p.distinct_overflow && p.cardinality() == Cardinality::Table);
	}

	#[test]
	fn char_classes_and_length() {
		assert_eq!(
			feed(&["USD", "CAD", "EUR"], 32).profile().classes.label(),
			Some("uppercase letters only")
		);
		assert_eq!(
			feed(&["active", "closed"], 32).profile().classes.label(),
			Some("lowercase letters only")
		);
		let p = feed(&["ab", "abcd", "a"], 32).profile();
		assert_eq!(p.len_range, Some((1, 4)));
	}

	#[test]
	fn profile_counts_nulls_and_temporal_range() {
		let p = feed(&["x", "", "y", ""], 32).profile();
		assert_eq!((p.non_null, p.null_count), (2, 2));
		let p = feed(&["2021-12-31", "2019-05-05", "2020-01-01"], 32).profile();
		assert_eq!(
			p.temporal_range,
			Some(("2019-05-05".to_owned(), "2021-12-31".to_owned()))
		);
		assert_eq!(p.num_range, None);
	}

	#[test]
	fn strict_header_check() {
		let first = headers(&["a", "b"]);
		// Order may differ.
		assert!(check_headers("f", &first, &headers(&["b", "a"])).is_ok());
		// A dropped or added column is an error.
		assert!(check_headers("f", &first, &headers(&["a"])).is_err());
		assert!(check_headers("f", &first, &headers(&["a", "b", "c"])).is_err());
	}

	#[test]
	fn identifiers_are_safe_and_unique() {
		let mut used = BTreeSet::new();
		assert_eq!(
			sanitize_ident("Account Number", &mut used, false),
			"account_number"
		);
		assert_eq!(
			sanitize_ident("Account-Number", &mut used, false),
			"account_number_2"
		);
		assert_eq!(sanitize_ident("123abc", &mut used, false), "c_123abc");
		// Attribute idents dodge the structural column names.
		assert_eq!(sanitize_ident("during", &mut used, true), "during_val");
	}
}
