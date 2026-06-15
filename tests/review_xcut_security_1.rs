//! xcut-security batch — static-invariant reproduction for the Telegram
//! bot-token leak in quickstart.
//!
//! Finding: `src/quickstart.rs` builds Bot API URLs with the token embedded in
//! the URL path (`https://api.telegram.org/bot{token}/...`) and calls
//! `reqwest::get(&url).await?`. reqwest's `Error` `Display` includes the full
//! request URL and only redacts userinfo (`user:pass@`), NOT path segments —
//! so the token in the path survives. The `?` propagates that error up through
//! `verify_bot` / `detect_group` / `verify_bot_is_admin`, and the four call
//! sites print it verbatim (`println!("{e}")`), leaking the bot token to the
//! operator's terminal (and any captured logs / pasted bug reports) on ANY
//! network failure (offline, DNS, proxy, TLS, Telegram blocked).
//!
//! Verified empirically that reqwest's `Display` echoes the token:
//!   error sending request for url (http://.../bot<TOKEN>/getMe)
//! and that `.without_url()` strips it. The daemon GitHub path
//! (`daemon/task_sweep.rs`) does this correctly — token in an `Authorization`
//! header, never in the URL.
//!
//! This runtime path cannot be driven deterministically from a test (the host
//! `api.telegram.org` is hard-coded, so there is no seam to force the
//! transport-error branch offline-deterministically). So this is a SOURCE-
//! SCANNING invariant: assert the token is NOT interpolated into a Telegram
//! URL that is then handed to a bare `reqwest::get`. The fix (relocate the
//! token into an `Authorization` header like `task_sweep`, or route through a
//! client whose error path is scrubbed / `without_url`) removes the
//! `bot{token}` interpolation from the URL string.
//!
//! RED now: the `api.telegram.org/bot{token}` URL templates are present.
//! GREEN after the fix stops embedding the token in the URL.

use std::path::PathBuf;

#[test]
fn quickstart_does_not_embed_bot_token_in_telegram_url_xcut_security() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/quickstart.rs");
    let text = std::fs::read_to_string(&path).expect("read src/quickstart.rs");

    // The dangerous construct: the bot token interpolated directly into the
    // Telegram URL path. reqwest's `Display` echoes this URL (with the token)
    // when the request errors, and the four print sites surface that error
    // verbatim. Any of the suggested fixes removes the token from the URL
    // string.
    let needle = "api.telegram.org/bot{token}";

    let mut violations = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        // Skip comment / doc lines that merely describe the pattern.
        if t.starts_with("//") || t.starts_with('*') || t.starts_with("//!") {
            continue;
        }
        if line.contains(needle) {
            violations.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
        }
    }

    assert!(
        violations.is_empty(),
        "quickstart embeds the bot token in the Telegram URL path; reqwest's \
         Error Display echoes the full URL (incl. token) on any network error, \
         and the call sites print it verbatim — leaking the token to the \
         terminal / logs / bug reports. Move the token to an Authorization \
         header (mirror daemon/task_sweep.rs) or scrub the error \
         (e.without_url()) before printing:\n{}",
        violations.join("\n")
    );
}
