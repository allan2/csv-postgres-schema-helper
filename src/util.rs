//! Small helpers shared between inference and loading.

/// A CSV cell is treated as SQL `NULL` when it is empty or matches one of
/// the caller-configured null tokens (e.g. `\N`, `NA`). Keeping this in
/// one place guarantees inference and loading agree on what counts as a
/// missing value.
pub fn is_null(value: &str, null_tokens: &[String]) -> bool {
	value.is_empty() || null_tokens.iter().any(|t| t == value)
}
