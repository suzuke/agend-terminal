//! [perf-audit R3 — t-…84833-14] MANUAL bench (NOT a CI test).
//!
//! Every fn here is `#[ignore]`d, so `cargo test` / the flake-gate never run it
//! (no criterion — deterministic guard is the `perf_r3_equiv` suite, not timing).
//! Run it by hand:
//!
//!   env -u AGEND_INSTANCE_NAME AGEND_GIT_BYPASS=1 \
//!     cargo test --release --bin agend-terminal \
//!     inbox::storage::perf_r3_bench -- --ignored --nocapture
//!
//! Isolates the COUNTING-LOOP cost that the unread-count path adds over the bare
//! O(1) append (the fsync'd append is identical across strategies → excluded).
//! `probe_struct` measures the SHIPPED `super::count_unread_in_content`.

use crate::inbox::InboxMessage;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

/// The pre-refactor baseline: full `InboxMessage` deserialize per line.
fn count_full_struct(content: &str) -> usize {
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| {
            serde_json::from_str::<InboxMessage>(l)
                .map(|m| {
                    m.read_at.is_none() && m.delivering_at.is_none() && m.superseded_by.is_none()
                })
                .unwrap_or(false)
        })
        .count()
}

/// Rejected alternative (kept for the comparison number): no-serde substring
/// scan. Faster but format-coupled / field-omission fragile — see the manifest.
fn count_byte_scan(content: &str) -> usize {
    content
        .lines()
        .filter(|l| {
            l.contains("\"read_at\":null")
                && !l.contains("\"delivering_at\":")
                && !l.contains("\"superseded_by\":")
        })
        .count()
}

fn build_fixture(total: usize, unread_tail: usize) -> (String, usize) {
    let mut lines: Vec<String> = Vec::with_capacity(total);
    let mut expected = 0usize;
    let read_rows = total.saturating_sub(unread_tail);
    for i in 0..read_rows {
        let mut m = InboxMessage {
            schema_version: 1,
            id: Some(format!("m-read-{i}")),
            from: "from:fixup-lead".to_string(),
            text: format!(
                "[report] wave {i} progress: PR landed, CI green, reviewer VERIFIED. {}",
                "context ".repeat(28)
            ),
            kind: Some(if i % 2 == 0 { "report" } else { "update" }.to_string()),
            timestamp: "2026-06-19T00:00:00Z".to_string(),
            read_at: Some("2026-06-19T00:01:00Z".to_string()),
            ..Default::default()
        };
        if i % 50 == 0 {
            m.superseded_by = Some("m-newer".to_string());
        }
        lines.push(serde_json::to_string(&m).expect("serialize read row"));
    }
    for i in 0..2.min(total) {
        let m = InboxMessage {
            schema_version: 1,
            id: Some(format!("m-delivering-{i}")),
            from: "from:fixup-lead".to_string(),
            text: "[task] in-flight dispatch being processed".to_string(),
            kind: Some("task".to_string()),
            timestamp: "2026-06-19T00:02:00Z".to_string(),
            delivering_at: Some("2026-06-19T00:02:30Z".to_string()),
            ..Default::default()
        };
        lines.push(serde_json::to_string(&m).expect("serialize delivering row"));
    }
    for i in 0..unread_tail {
        let m = InboxMessage {
            schema_version: 1,
            id: Some(format!("m-unread-{i}")),
            from: "from:fixup-lead".to_string(),
            text: format!(
                "[delegate_task] perf-audit subtask: spike then impl. {}",
                "detailed multi-paragraph dispatch description ".repeat(30)
            ),
            kind: Some("task".to_string()),
            timestamp: "2026-06-19T00:03:00Z".to_string(),
            ..Default::default()
        };
        expected += 1;
        lines.push(serde_json::to_string(&m).expect("serialize unread row"));
    }
    (lines.join("\n") + "\n", expected)
}

fn bench<F: Fn() -> usize>(iters: usize, f: F) -> f64 {
    for _ in 0..(iters / 10).max(1) {
        black_box(f());
    }
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t0 = Instant::now();
        for _ in 0..iters {
            black_box(f());
        }
        let per = t0.elapsed().as_nanos() as f64 / iters as f64;
        if per < best {
            best = per;
        }
    }
    best
}

#[test]
#[ignore = "manual perf bench — run with --ignored --nocapture"]
fn bench_inbox_unread_count_strategies() {
    println!("\n=== [R3] inbox unread-count strategy bench (ns/op, best-of-5) ===");
    println!("(count loop only; fsync'd append is identical across strategies, excluded)\n");
    println!(
        "{:>6} | {:>10} | {:>12} | {:>14} | {:>12} | {:>8}",
        "rows", "read_only", "full(old)", "probe(SHIPPED)", "byte_scan", "unread"
    );
    println!("{}", "-".repeat(80));

    for &total in &[0usize, 50, 300, 1000] {
        let unread_tail = if total == 0 { 0 } else { 8.min(total) };
        let (body, expected) = build_fixture(total, unread_tail);

        let path: PathBuf = std::env::temp_dir().join(format!(
            "agend-r3-bench-{}-{}.jsonl",
            total,
            std::process::id()
        ));
        std::fs::write(&path, &body).expect("write fixture");
        let _ = std::fs::read_to_string(&path).expect("warm read");

        // shipped probe == old full == rejected byte-scan, on the prod fixture.
        let c_full = count_full_struct(&body);
        let c_probe = super::count_unread_in_content(&body);
        let c_byte = count_byte_scan(&body);
        assert_eq!(c_full, expected);
        assert_eq!(
            c_probe, c_full,
            "SHIPPED probe must equal old full-struct count"
        );
        assert_eq!(c_byte, c_full);

        let iters = (200_000 / total.max(1) * 10).clamp(2_000, 200_000);
        let t_read = bench(iters, || {
            black_box(std::fs::read_to_string(&path).expect("read").len())
        });
        let t_full = bench(iters, || count_full_struct(&body));
        let t_probe = bench(iters, || super::count_unread_in_content(&body));
        let t_byte = bench(iters, || count_byte_scan(&body));

        println!(
            "{:>6} | {:>10.0} | {:>12.0} | {:>14.0} | {:>12.0} | {:>8}",
            total, t_read, t_full, t_probe, t_byte, expected
        );
        std::fs::remove_file(&path).ok();
    }
    println!();
}
