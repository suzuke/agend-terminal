#[cfg(test)]
mod tests {
    use std::cell::Cell;

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

    #[tracing_test::traced_test]
    fn hard_limit_below_target_is_used_and_warned() {
        let requested = Cell::new(None);
        let effective = super::apply_nofile_limit_with(
            65_536,
            || {
                Ok(super::NofileLimits {
                    soft: 256,
                    hard: Some(2_048),
                })
            },
            |target| {
                requested.set(Some(target));
                Ok(super::NofileLimits {
                    soft: target,
                    hard: Some(2_048),
                })
            },
        );

        assert_eq!(requested.get(), Some(2_048));
        assert_eq!(effective.expect("limit query succeeds").soft, 2_048);
        assert!(logs_contain("below recommended minimum"));
    }

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
