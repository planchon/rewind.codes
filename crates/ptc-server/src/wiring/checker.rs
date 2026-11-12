//! Type-checking: `ptc-check` (tsgo), behind the `TypeChecker` port (`archi/04-tools.md`,
//! "Type-checking"). tsgo is embedded in the binary and extracted to a cache directory at its
//! first use.

use std::path::PathBuf;

use ptc_check::TsgoChecker;

/// The checker of starts, forks and `POST /v1/check`.
pub(crate) type Checker = TsgoChecker;

/// A checker extracting tsgo into `cache`, or the user's cache directory.
pub(crate) fn checker(cache: Option<PathBuf>) -> Checker {
    TsgoChecker::new(cache.unwrap_or_else(TsgoChecker::default_cache_dir))
}
