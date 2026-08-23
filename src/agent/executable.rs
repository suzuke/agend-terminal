use std::path::PathBuf;

/// Resolve a backend command and pin the concrete executable selected now.
///
/// Resolving first avoids Windows selecting an extensionless npm shell script
/// instead of its `.cmd` wrapper. Canonicalising then makes validation and exec
/// use the same retained versioned binary when an updater flips a launcher
/// symlink between those operations (#3329).
pub(super) fn resolve_and_pin(command: &str) -> PathBuf {
    let resolved = which::which(command).unwrap_or_else(|_| PathBuf::from(command));
    // `dunce` strips Windows' verbatim-path prefix so CreateProcessW accepts it.
    dunce::canonicalize(&resolved).unwrap_or(resolved)
}
