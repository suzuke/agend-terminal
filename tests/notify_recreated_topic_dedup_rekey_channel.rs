//! channel-LOW-4 repro (static invariant): when `notify_telegram_inner`'s
//! topic-deleted recovery recreates the topic and retries the send to `new_tid`,
//! it must ALSO record (re-key) a dedup claim under the NEW topic id — otherwise
//! the documented dedup invariant ("suppress same content to same topic within
//! TTL") is not maintained across a topic recreation.
//!
//! WHY THIS IS A BUG: the dedup claim is recorded BEFORE the send under
//! `dedup_key`, which is built from the OLD `topic_id`
//! (`crate::channel::dedup::DedupKey::new("telegram:notify", instance, topic_id.map(i64::from), text)`).
//! On a topic-deleted error the code recreates the topic and retries the send to
//! `new_tid`, but it never records a claim keyed on `Some(new_tid)`. So a
//! concurrent SECOND emission targeting the recreated topic computes the
//! `(instance, Some(new_tid), content)` tuple, finds NO matching claim, and is
//! NOT suppressed — the RC2/RC3 dedup guarantee that motivated
//! record-before-send lapses precisely across the recreated-topic window.
//!
//! CORRECT BEHAVIOR (the fix): after a successful retry to `new_tid`, also
//! `record_and_check` (or re-key) a dedup claim for
//! `(channel, instance, Some(new_tid), text)`, so a concurrent emission to the
//! recreated topic is still deduped. After the fix, `notify.rs` contains a dedup
//! record/re-key op on a line that references `new_tid`.
//!
//! METHOD: a SOURCE-SCANNING invariant. A behavioral test cannot drive this path
//! without the fix: the retry lives inside the fire-and-forget `spawn` onto the
//! undriven `telegram_runtime()` (see channel-HIGH-1), AND it requires
//! `invalidate_and_recreate_topic` to succeed (a live `create_forum_topic` API
//! call) followed by a successful retry send — neither reachable offline today.
//! So this guard pins the structural property the fix establishes. RED now (no
//! dedup op references `new_tid`); GREEN once the retry re-keys the claim.

use std::path::PathBuf;

#[test]
#[ignore = "channel-LOW-4: red until fix; remove #[ignore] after fix to confirm"]
fn notify_recreated_topic_retry_rekeys_dedup_claim_channel() {
    let notify = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/channel/telegram/notify.rs");
    let content = std::fs::read_to_string(&notify)
        .expect("channel-LOW-4: src/channel/telegram/notify.rs must exist");

    // A dedup record / re-key operation on a line that also references the
    // recreated topic id `new_tid`. The fix adds exactly such a line in the
    // topic-recreate retry block; today there is none (the only dedup op in that
    // block is `.evict(&dedup_key)`, which uses the OLD key — a rollback, not a
    // re-key under the NEW topic).
    let rekeys_new_topic = content.lines().any(|line| {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            return false; // skip comment / doc lines
        }
        line.contains("new_tid")
            && (line.contains("record_and_check") || line.contains("DedupKey::new"))
    });

    assert!(
        rekeys_new_topic,
        "channel-LOW-4: the topic-deleted recovery in `notify_telegram_inner` \
         retries the send to the recreated topic (`new_tid`) but never records a \
         dedup claim keyed on `Some(new_tid)`. A concurrent duplicate emission to \
         the recreated topic is therefore NOT suppressed — the dedup invariant \
         lapses across topic recreation. After a successful retry, also \
         `record_and_check` a `DedupKey::new(\"telegram:notify\", instance, \
         Some(new_tid), text)` so the recreated-topic window stays deduped."
    );
}
