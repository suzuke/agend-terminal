//! API handler for verify-push (push-time claim verification).

use serde_json::{json, Value};

pub(crate) fn handle_verify_push(params: &Value) -> Value {
    let base = match params["base"].as_str() {
        Some(b) => b,
        None => return json!({"ok": false, "error": "missing 'base' param"}),
    };
    let head = params["head"].as_str().unwrap_or("HEAD");
    let claim_text = match params["claim"].as_str() {
        Some(c) => c,
        None => return json!({"ok": false, "error": "missing 'claim' param"}),
    };
    let repo_dir = params["repo_dir"]
        .as_str()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let claims = crate::claim_verifier::parse_claims(claim_text);
    let result = crate::claim_verifier::verify(&repo_dir, base, head, &claims);

    json!({
        "ok": result.ok,
        "results": result.results,
    })
}
