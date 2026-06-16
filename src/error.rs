//! Hand-rolled error type (no `thiserror`/`anyhow`, per project style).

use std::{fmt, path::PathBuf};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum Error {
	/// Filesystem I/O, including opening a CSV or the manifest list.
	Io(std::io::Error),
	/// CSV parse failure from the `csv` crate.
	Csv(csv::Error),
	/// Any error surfaced by `tokio_postgres`.
	Pg(tokio_postgres::Error),
	/// A malformed line in the manifest (the plain-text list of CSVs).
	Manifest {
		path: PathBuf,
		line: usize,
		message: String,
	},
	/// Inference could not produce a usable schema (no columns, key
	/// column not found, and so on).
	Schema(String),
	/// A strict-mode data problem in a specific input file (ragged row,
	/// blank account key, headers that disagree with earlier files).
	Data { source: String, message: String },
	/// A required environment variable was absent or empty.
	MissingEnv(&'static str),
	/// Command-line arguments did not parse.
	Usage(String),
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Io(e) => write!(f, "io error: {e}"),
			Self::Csv(e) => write!(f, "csv error: {e}"),
			Self::Pg(e) => write!(f, "postgres error: {e}"),
			Self::Manifest {
				path,
				line,
				message,
			} => write!(f, "{}:{line}: {message}", path.display()),
			Self::Schema(m) => write!(f, "schema error: {m}"),
			Self::Data { source, message } => write!(f, "{source}: {message}"),
			Self::MissingEnv(name) => write!(f, "missing environment variable {name}"),
			Self::Usage(m) => write!(f, "{m}"),
		}
	}
}

impl std::error::Error for Error {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Io(e) => Some(e),
			Self::Csv(e) => Some(e),
			Self::Pg(e) => Some(e),
			Self::Manifest { .. }
			| Self::Schema(_)
			| Self::Data { .. }
			| Self::MissingEnv(_)
			| Self::Usage(_) => None,
		}
	}
}

impl From<std::io::Error> for Error {
	fn from(e: std::io::Error) -> Self {
		Self::Io(e)
	}
}

impl From<csv::Error> for Error {
	fn from(e: csv::Error) -> Self {
		Self::Csv(e)
	}
}

impl From<tokio_postgres::Error> for Error {
	fn from(e: tokio_postgres::Error) -> Self {
		Self::Pg(e)
	}
}
