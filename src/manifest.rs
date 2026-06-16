//! Parse the plain-text list of CSVs to process, in order.
//!
//! Each non-blank, non-`#` line is `PATH [WHEN]`:
//! - `PATH` is a CSV path (no embedded spaces), resolved relative to the
//!   manifest's own directory when not absolute.
//! - `WHEN` is an optional observation instant — a `YYYY-MM-DD` date or a
//!   full timestamp — that becomes the lower bound of the temporal range
//!   for everything loaded from that file.
//!
//! If `WHEN` is omitted we try to lift a `YYYY-MM` / `YYYY-MM-DD` out of
//! the path. If no line carries a usable date at all, we fall back to
//! synthetic consecutive months (2000-01, 2000-02, …) so the README's
//! bare `month1/a.csv` list still loads with monotonic, non-overlapping
//! ranges.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// One file to process, with its resolved path, the original path string
/// (kept verbatim as load provenance), and the observation instant as a
/// string ready to cast to `timestamptz`.
pub struct Entry {
	pub path: PathBuf,
	pub source: String,
	pub at: String,
}

/// A manifest line before the date fallback is resolved: `at` is `None`
/// when neither an explicit field nor the path supplied one.
struct Raw {
	path: PathBuf,
	source: String,
	at: Option<String>,
}

pub fn parse(list_path: &Path) -> Result<Vec<Entry>> {
	let text = std::fs::read_to_string(list_path)?;
	let base = list_path.parent().unwrap_or_else(|| Path::new("."));

	let mut raws: Vec<Raw> = Vec::new();
	for (i, line) in text.lines().enumerate() {
		let line = line.trim();
		if line.is_empty() || line.starts_with('#') {
			continue;
		}
		let mut parts = line.splitn(2, char::is_whitespace);
		let source = parts.next().unwrap_or("").to_owned();
		let when = parts.next().map(str::trim).filter(|s| !s.is_empty());
		let at = match when {
			Some(w) if is_instant(w) => Some(w.to_owned()),
			Some(w) => {
				return Err(Error::Manifest {
					path: list_path.to_path_buf(),
					line: i + 1,
					message: format!("`{w}` is not a YYYY-MM-DD date or timestamp"),
				});
			}
			None => date_from_path(&source),
		};
		let path = {
			let p = Path::new(&source);
			if p.is_absolute() {
				p.to_path_buf()
			} else {
				base.join(p)
			}
		};
		raws.push(Raw { path, source, at });
	}

	if raws.is_empty() {
		return Err(Error::Manifest {
			path: list_path.to_path_buf(),
			line: 0,
			message: "manifest lists no files".to_owned(),
		});
	}

	// If nobody supplied a date, synthesize consecutive months. Otherwise
	// every line must resolve to one.
	let any_dated = raws.iter().any(|r| r.at.is_some());
	let mut entries = Vec::with_capacity(raws.len());
	for (i, raw) in raws.into_iter().enumerate() {
		let at = match raw.at {
			Some(a) => a,
			None if any_dated => {
				return Err(Error::Manifest {
					path: list_path.to_path_buf(),
					line: i + 1,
					message: format!(
						"no date for `{}` (other lines have dates, so this one needs one too)",
						raw.source
					),
				});
			}
			None => synthetic_month(i),
		};
		entries.push(Entry {
			path: raw.path,
			source: raw.source,
			at,
		});
	}
	Ok(entries)
}

/// `2000-01-01`, `2000-02-01`, … one month apart, in list order.
fn synthetic_month(index: usize) -> String {
	let year = 2000 + index / 12;
	let month = 1 + index % 12;
	format!("{year:04}-{month:02}-01")
}

fn is_instant(s: &str) -> bool {
	is_ymd(s) || (s.len() > 10 && is_ymd(&s[..10]) && s.contains(':'))
}

fn is_ymd(s: &str) -> bool {
	let b = s.as_bytes();
	b.len() == 10
		&& b[4] == b'-'
		&& b[7] == b'-'
		&& (0..4).all(|i| b[i].is_ascii_digit())
		&& (5..7).all(|i| b[i].is_ascii_digit())
		&& (8..10).all(|i| b[i].is_ascii_digit())
}

/// Pull the first `YYYY-MM` or `YYYY-MM-DD` out of a path. A bare
/// `YYYY-MM` becomes the first of that month.
fn date_from_path(source: &str) -> Option<String> {
	let b = source.as_bytes();
	let mut i = 0;
	while i + 7 <= b.len() {
		if (i..i + 4).all(|j| b[j].is_ascii_digit())
			&& b[i + 4] == b'-'
			&& b[i + 5].is_ascii_digit()
			&& b[i + 6].is_ascii_digit()
		{
			let has_day = i + 10 <= b.len()
				&& b[i + 7] == b'-'
				&& b[i + 8].is_ascii_digit()
				&& b[i + 9].is_ascii_digit();
			return Some(if has_day {
				source[i..i + 10].to_owned()
			} else {
				format!("{}-01", &source[i..i + 7])
			});
		}
		i += 1;
	}
	None
}

#[cfg(test)]
mod tests {
	use super::{date_from_path, is_instant, synthetic_month};

	#[test]
	fn lifts_dates_from_paths() {
		assert_eq!(
			date_from_path("data/2024-03-15/a.csv").as_deref(),
			Some("2024-03-15")
		);
		assert_eq!(
			date_from_path("export-2024-03.csv").as_deref(),
			Some("2024-03-01")
		);
		assert_eq!(date_from_path("month1/a.csv"), None);
	}

	#[test]
	fn synthetic_months_roll_over_the_year() {
		assert_eq!(synthetic_month(0), "2000-01-01");
		assert_eq!(synthetic_month(12), "2001-01-01");
	}

	#[test]
	fn instants() {
		assert!(is_instant("2024-01-01") && is_instant("2024-01-01T09:00:00Z"));
		assert!(!is_instant("2024-1-1") && !is_instant("hello"));
	}
}
