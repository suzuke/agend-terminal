//! Shared numeric-env parsing — one source of truth for the
//! `env::var(N).ok().and_then(parse).unwrap_or(D)` idiom that was copy-pasted
//! across ~a dozen sites (most of which #1819 has since demoted to consts).
//!
//! The behavioural change versus the scattered copies: a malformed value is now
//! **consistently** `warn`-logged (previously only `boot_sweep` warned; the rest
//! silently fell back to the default). Default-on-unset and the parsed value are
//! byte-identical to the inline code these replaced.

use std::fmt::Display;
use std::str::FromStr;

/// Parse env var `name` as `T`. Unset → `default`. Malformed → `default` plus a
/// one-line `warn` so an operator typo is visible instead of silently ignored.
pub(crate) fn env_parse<T>(name: &str, default: T) -> T
where
    T: FromStr + Display + Copy,
{
    match std::env::var(name) {
        Ok(raw) => match raw.parse::<T>() {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!(
                    env = name,
                    value = %raw,
                    "malformed numeric env value — using default"
                );
                default
            }
        },
        Err(_) => default,
    }
}

/// Like [`env_parse`] but a parsed value that compares `< min` falls back to
/// `default` (the `…filter(|v| *v >= min).unwrap_or(default)` idiom). A
/// below-min value is NOT warned (it parsed fine — it's just out of range,
/// matching the prior silent `filter` behaviour); only a malformed value warns.
pub(crate) fn env_parse_min<T>(name: &str, default: T, min: T) -> T
where
    T: FromStr + Display + Copy + PartialOrd,
{
    match std::env::var(name) {
        Ok(raw) => match raw.parse::<T>() {
            Ok(v) if v >= min => v,
            Ok(_) => default,
            Err(_) => {
                tracing::warn!(
                    env = name,
                    value = %raw,
                    "malformed numeric env value — using default"
                );
                default
            }
        },
        Err(_) => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env mutation is process-global and races across all keys — serialise via
    /// the crate-wide test lock (#1812).
    fn with_env<R>(name: &str, value: Option<&str>, f: impl FnOnce() -> R) -> R {
        let _g = crate::daemon::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var(name).ok();
        // SAFETY: serialised by the lock above; test-only.
        unsafe {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
        let r = f();
        unsafe {
            match prior {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
        r
    }

    const K: &str = "AGEND_TEST_ENV_UTIL_FIXTURE";

    #[test]
    fn env_parse_unset_returns_default() {
        with_env(K, None, || assert_eq!(env_parse::<u64>(K, 42), 42));
    }

    #[test]
    fn env_parse_valid_returns_parsed() {
        with_env(K, Some("7"), || assert_eq!(env_parse::<u64>(K, 42), 7));
    }

    #[test]
    fn env_parse_malformed_returns_default() {
        with_env(K, Some("not-a-number"), || {
            assert_eq!(env_parse::<u64>(K, 42), 42)
        });
    }

    #[test]
    fn env_parse_works_for_i64_and_usize() {
        with_env(K, Some("-3"), || assert_eq!(env_parse::<i64>(K, 9), -3));
        with_env(K, Some("5"), || assert_eq!(env_parse::<usize>(K, 9), 5));
    }

    #[test]
    fn env_parse_min_below_min_falls_back_to_default() {
        // 0 < min(1) → default; valid >= min → value; malformed → default.
        with_env(K, Some("0"), || {
            assert_eq!(env_parse_min::<i64>(K, 7, 1), 7)
        });
        with_env(K, Some("3"), || {
            assert_eq!(env_parse_min::<i64>(K, 7, 1), 3)
        });
        with_env(K, Some("garbage"), || {
            assert_eq!(env_parse_min::<i64>(K, 7, 1), 7)
        });
        with_env(K, None, || assert_eq!(env_parse_min::<i64>(K, 7, 1), 7));
    }
}
