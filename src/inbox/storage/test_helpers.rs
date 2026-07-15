use std::path::Path;

use super::inbox_path_resolved;

/// Test-only: force one row's `delivering_at` to an explicit rfc3339 timestamp
/// so reclaim tests can age a row without a wall-clock sleep.
pub(crate) fn set_row_delivering_at_for_test(
    home: &Path,
    name: &str,
    msg_id: &str,
    ts_rfc3339: &str,
) {
    let path = inbox_path_resolved(home, name);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            panic!("set_row_delivering_at_for_test: cannot read {name}'s inbox at {path:?}: {e}")
        }
    };
    let mut out = String::new();
    let mut aged = 0usize;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(mut v) => {
                if v.get("id").and_then(|x| x.as_str()) == Some(msg_id) {
                    v["delivering_at"] = serde_json::Value::String(ts_rfc3339.to_string());
                    aged += 1;
                }
                out.push_str(&serde_json::to_string(&v).unwrap_or_else(|_| line.to_string()));
            }
            Err(_) => out.push_str(line),
        }
        out.push('\n');
    }
    assert_eq!(
        aged, 1,
        "set_row_delivering_at_for_test: expected exactly one row with id={msg_id} in {name}'s inbox, aged {aged}"
    );
    if let Err(e) = std::fs::write(&path, out) {
        panic!("set_row_delivering_at_for_test: cannot write {name}'s inbox: {e}");
    }
}
