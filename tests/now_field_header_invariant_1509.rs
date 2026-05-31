//! #1509 invariant: every `[AGEND-MSG` PTY-header BUILDER in `src/inbox/notify.rs`
//! must stamp the operator-TZ `now=` field. #1487 added `now=` to three of the
//! four formatters and missed `format_notification_for_inject`, so telegram-
//! inbound and the daemon `notify_agent` notices shipped without it. This guards
//! against a future formatter dropping `now=` again.
//!
//! Precise (FP-free) rule: a header BUILDER is a top-level fn that RETURNS
//! `String` AND references a header prefix (the `[AGEND-MSG` literal, or the
//! `HEADER_PREFIX` / `PENDING_HEADER_PREFIX` consts). Consumers/matchers return
//! `bool`/`Option` and are excluded. A builder must contain a now-token:
//! `operator_now_field` (computes it), `now_field` (receives it), or a literal
//! `now=`. The body-replace inline form (no `[AGEND-MSG]` header) is out of
//! scope by design (see the #1509 NOTE in `format_notification_for_inject`).

fn is_fn_opener(line: &str) -> bool {
    // Top-level fn (column 0): `fn `, `pub fn `, `pub(crate) fn `, `pub(super) fn `.
    line.starts_with("fn ")
        || line.starts_with("pub fn ")
        || line.starts_with("pub(crate) fn ")
        || line.starts_with("pub(super) fn ")
}

#[test]
fn every_agend_msg_header_builder_stamps_now_1509() {
    let src = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/inbox/notify.rs"),
    )
    .expect("read src/inbox/notify.rs");

    // Only scan production code — stop at the first test module.
    let prod = match src.find("\n#[cfg(test)]") {
        Some(i) => &src[..i],
        None => &src[..],
    };

    let prefix_markers = ["[AGEND-MSG", "HEADER_PREFIX", "PENDING_HEADER_PREFIX"];
    // CODE identifiers only — not the literal `now=`, which appears in comments
    // (e.g. the body-branch NOTE) and would falsely satisfy the check. Every real
    // builder either calls `operator_now_field()` or receives a `now_field` arg.
    let now_tokens = ["operator_now_field", "now_field"];

    // Strip line comments so a doc/inline mention of a prefix or token can't
    // masquerade as code (split on `//` — notify.rs format strings contain none).
    let prod: String = prod
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    let prod = prod.as_str();

    // Chunk into top-level fn bodies: from one column-0 fn opener to the next.
    let lines: Vec<&str> = prod.lines().collect();
    let mut openers: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_fn_opener(l))
        .map(|(i, _)| i)
        .collect();
    openers.push(lines.len()); // sentinel end

    let mut violations = Vec::new();
    for win in openers.windows(2) {
        let (start, end) = (win[0], win[1]);
        let chunk = lines[start..end].join("\n");
        // A header builder: returns String AND references a header prefix.
        let returns_string = chunk.contains("-> String");
        let builds_header = prefix_markers.iter().any(|m| chunk.contains(m));
        if returns_string && builds_header {
            let has_now = now_tokens.iter().any(|t| chunk.contains(t));
            if !has_now {
                let sig = lines[start].trim();
                violations.push(format!("line {}: {sig}", start + 1));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "#1509: notify.rs has an `[AGEND-MSG` header builder (-> String) that does NOT \
         stamp `now=` (via operator_now_field / now_field). #1487's operator-TZ timestamp \
         must appear on every injected header:\n{}",
        violations.join("\n")
    );
}
