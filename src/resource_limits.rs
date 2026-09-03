//! Process file-descriptor limit setup and diagnostics.

use std::fmt::Display;
use std::io;
use std::path::Path;

const DEFAULT_NOFILE_LIMIT: u64 = 65_536;
const MIN_NOFILE_LIMIT: u64 = 4_096;
const NOFILE_LIMIT_ENV: &str = "AGEND_NOFILE_LIMIT";
const FD_USAGE_WARN_PERCENT: f64 = 80.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NofileLimits {
    soft: u64,
    /// `None` represents `RLIM_INFINITY`.
    hard: Option<u64>,
}

fn format_hard_limit(hard: Option<u64>) -> String {
    hard.map_or_else(|| "unlimited".to_string(), |value| value.to_string())
}

fn apply_nofile_limit_with<Get, Set>(
    desired: u64,
    get_limits: Get,
    set_soft_limit: Set,
) -> Option<NofileLimits>
where
    Get: FnOnce() -> io::Result<NofileLimits>,
    Set: FnOnce(u64) -> io::Result<NofileLimits>,
{
    let current = match get_limits() {
        Ok(limits) => limits,
        Err(error) => {
            tracing::warn!(
                %error,
                "failed to read file descriptor limit; continuing daemon startup"
            );
            return None;
        }
    };
    let target = current.hard.map_or(desired, |hard| desired.min(hard));
    if target < desired {
        tracing::warn!(
            soft = current.soft,
            hard = %format_hard_limit(current.hard),
            desired,
            target,
            below_recommended_minimum = target < MIN_NOFILE_LIMIT,
            "hard file descriptor limit caps desired target"
        );
    }

    let effective = if current.soft < target {
        match set_soft_limit(target) {
            Ok(limits) => {
                tracing::info!(
                    previous_soft = current.soft,
                    soft = limits.soft,
                    hard = %format_hard_limit(limits.hard),
                    desired,
                    "raised file descriptor limit"
                );
                limits
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    soft = current.soft,
                    hard = %format_hard_limit(current.hard),
                    desired,
                    target,
                    "failed to raise file descriptor limit; continuing daemon startup"
                );
                current
            }
        }
    } else {
        current
    };

    tracing::info!(
        soft = effective.soft,
        hard = %format_hard_limit(effective.hard),
        desired,
        "effective file descriptor limit"
    );
    Some(effective)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // libc::rlim_t width differs across Unix targets.
fn query_nofile_limits() -> io::Result<NofileLimits> {
    let mut raw = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `raw` is a valid writable rlimit and RLIMIT_NOFILE is supported
    // on every Unix target compiled by this crate.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut raw) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(NofileLimits {
        soft: raw.rlim_cur as u64,
        hard: (raw.rlim_max != libc::RLIM_INFINITY).then_some(raw.rlim_max as u64),
    })
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // libc::rlim_t width differs across Unix targets.
fn set_nofile_soft_limit(target: u64) -> io::Result<NofileLimits> {
    let current = query_nofile_limits()?;
    let raw = libc::rlimit {
        rlim_cur: target.min(libc::rlim_t::MAX as u64) as libc::rlim_t,
        rlim_max: current
            .hard
            .map_or(libc::RLIM_INFINITY, |hard| hard as libc::rlim_t),
    };
    // SAFETY: `raw` preserves the process hard limit and changes only the soft
    // RLIMIT_NOFILE value to a value capped by that hard limit.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw) } != 0 {
        return Err(io::Error::last_os_error());
    }
    query_nofile_limits()
}

/// Raise the daemon's inherited soft descriptor limit. This is deliberately
/// best-effort: diagnostics are emitted, but startup always continues.
#[cfg(unix)]
pub fn raise_daemon_nofile_limit() {
    let desired =
        crate::env_util::env_parse_min(NOFILE_LIMIT_ENV, DEFAULT_NOFILE_LIMIT, MIN_NOFILE_LIMIT);
    let _ = apply_nofile_limit_with(desired, query_nofile_limits, set_nofile_soft_limit);
}

#[cfg(not(unix))]
pub fn raise_daemon_nofile_limit() {
    tracing::info!("file descriptor limit adjustment is unavailable on this platform");
}

#[cfg(unix)]
fn open_fd_count() -> io::Result<u64> {
    let entries = std::fs::read_dir("/proc/self/fd").or_else(|_| std::fs::read_dir("/dev/fd"))?;
    let count = entries.filter_map(Result::ok).count() as u64;
    // The directory iterator itself occupies one descriptor and appears in the
    // directory listing on Linux and macOS; exclude that diagnostic-only handle.
    Ok(count.saturating_sub(1))
}

fn format_fd_usage(open: u64, soft: u64) -> String {
    let percent = if soft == 0 {
        100.0
    } else {
        open as f64 * 100.0 / soft as f64
    };
    let warning = if percent >= FD_USAGE_WARN_PERCENT {
        " — WARNING: near file descriptor limit"
    } else {
        ""
    };
    format!("{open}/{soft} ({percent:.1}%){warning}")
}

/// Human-readable current/soft descriptor usage for `doctor`.
pub fn doctor_fd_usage() -> String {
    #[cfg(unix)]
    {
        match (open_fd_count(), query_nofile_limits()) {
            (Ok(open), Ok(limits)) => format_fd_usage(open, limits.soft),
            (Err(error), _) | (_, Err(error)) => format!("unavailable ({error})"),
        }
    }
    #[cfg(not(unix))]
    {
        "unavailable on this platform".to_string()
    }
}

/// Persist descriptor exhaustion detected by daemon-wide error chokepoints.
///
/// Many callers expose an erased error type (`anyhow` or a domain wrapper), so
/// the stable OS message/code is the only common signal available here.
pub fn record_fd_exhaustion(home: &Path, operation: &str, error: &impl Display) -> bool {
    let rendered = error.to_string();
    let normalized = rendered.to_ascii_lowercase();
    if !normalized.contains("too many open files") && !normalized.contains("os error 24") {
        return false;
    }
    if let Err(log_error) = crate::event_log::try_log(
        home,
        "fd_exhausted",
        operation,
        &format!("{operation}: {rendered}"),
    ) {
        tracing::warn!(
            %log_error,
            operation,
            "failed to persist fd exhaustion event"
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    #[test]
    #[tracing_test::traced_test]
    fn low_soft_limit_is_raised_and_logged() {
        let requested = Cell::new(None);
        let effective = super::apply_nofile_limit_with(
            65_536,
            || {
                Ok(super::NofileLimits {
                    soft: 256,
                    hard: None,
                })
            },
            |target| {
                requested.set(Some(target));
                Ok(super::NofileLimits {
                    soft: target,
                    hard: None,
                })
            },
        );

        assert_eq!(requested.get(), Some(65_536));
        assert_eq!(effective.expect("limit query succeeds").soft, 65_536);
        assert!(logs_contain("raised file descriptor limit"));
        assert!(logs_contain("soft=65536"));
        assert!(logs_contain("hard=unlimited"));
    }

    #[test]
    #[tracing_test::traced_test]
    fn hard_limit_below_target_is_used_and_warned() {
        let requested = Cell::new(None);
        let effective = super::apply_nofile_limit_with(
            65_536,
            || {
                Ok(super::NofileLimits {
                    soft: 256,
                    hard: Some(8_192),
                })
            },
            |target| {
                requested.set(Some(target));
                Ok(super::NofileLimits {
                    soft: target,
                    hard: Some(8_192),
                })
            },
        );

        assert_eq!(requested.get(), Some(8_192));
        assert_eq!(effective.expect("limit query succeeds").soft, 8_192);
        assert!(logs_contain(
            "hard file descriptor limit caps desired target"
        ));
    }

    #[test]
    #[tracing_test::traced_test]
    fn set_failure_warns_and_keeps_startup_non_fatal() {
        let effective = super::apply_nofile_limit_with(
            65_536,
            || {
                Ok(super::NofileLimits {
                    soft: 256,
                    hard: None,
                })
            },
            |_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        );

        assert_eq!(
            effective.expect("get still reports current limit").soft,
            256
        );
        assert!(logs_contain("failed to raise file descriptor limit"));
        assert!(logs_contain("continuing daemon startup"));
    }

    #[test]
    fn doctor_warns_when_fd_usage_is_near_soft_limit() {
        let line = super::format_fd_usage(900, 1_000);

        assert!(line.contains("900/1000"));
        assert!(line.contains("90.0%"));
        assert!(line.contains("WARNING: near file descriptor limit"));
    }

    #[test]
    fn emfile_is_persisted_to_the_event_log() {
        let home = std::env::temp_dir().join(format!(
            "agend-fd-event-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).expect("temp home");
        let error = std::io::Error::from_raw_os_error(libc::EMFILE);

        assert!(super::record_fd_exhaustion(&home, "api_accept", &error));
        let log =
            std::fs::read_to_string(home.join("event-log.jsonl")).expect("fd exhaustion event log");
        assert!(log.contains("fd_exhausted"));
        assert!(log.contains("api_accept"));
        assert!(log.contains("Too many open files") || log.contains("os error 24"));
        std::fs::remove_dir_all(home).ok();
    }
}
