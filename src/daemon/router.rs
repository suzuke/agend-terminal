//! Sprint 52 router-layer — observes agent PTY output and mirrors to
//! the originating channel when `reply_to` is set.
//!
//! Lock ordering: router thread NEVER acquires L1 (registry) or L2 (agent_core).
//! It reads from crossbeam subscriber channels (no lock) and writes only to
//! L3 (heartbeat_pair, leaf-level). Mirror dispatch uses channel::send_from_agent
//! (no daemon lock involved).
//!
//! Subscriber registration happens at agent spawn time (caller already holds
//! L1/L2). The router receives new agent channels via a registration channel.

use crate::agent::AgentRegistry;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Silence timeout for end-of-turn fallback (3s per operator §13.4).
const SILENCE_TIMEOUT: Duration = Duration::from_secs(3);
/// Maximum mirror text length (chars) to dispatch.
const MAX_MIRROR_LEN: usize = 4000;

/// Byte index at which to start draining the mirror buffer to keep it under
/// `MAX_MIRROR_LEN`, snapped forward to the next char boundary so we never slice
/// a multi-byte UTF-8 char (e.g. Chinese) mid-character. `String::drain(..n)`
/// panics when `n` is not a char boundary (router.rs char_boundary panics on
/// long Chinese turns, 2026-06-12).
fn mirror_drain_start(buf: &str) -> usize {
    let mut drain = buf.len().saturating_sub(MAX_MIRROR_LEN);
    while drain < buf.len() && !buf.is_char_boundary(drain) {
        drain += 1;
    }
    drain
}

/// Cap the mirror buffer in place: once it grows past `2 * MAX_MIRROR_LEN`,
/// drain the front back under the cap, starting at the char-boundary-snapped
/// index from [`mirror_drain_start`] so a multi-byte char is never split (the
/// #2084 `drain(..n)` panic on a non-boundary `n`). Extracted verbatim from the
/// run-loop per-event consume so the cap-drain path is directly unit-testable.
fn apply_mirror_cap(buf: &mut String) {
    if buf.len() > MAX_MIRROR_LEN * 2 {
        let drain = mirror_drain_start(buf);
        buf.drain(..drain);
    }
}

/// Registration message: agent name + PTY output receiver.
pub struct AgentSubscription {
    pub name: String,
    pub rx: crossbeam_channel::Receiver<Vec<u8>>,
}

/// Global registration channel for new agent subscriptions.
/// Agents register at spawn time; router consumes registrations.
static REGISTRATION_TX: OnceLock<crossbeam_channel::Sender<AgentSubscription>> = OnceLock::new();

/// Register a new agent's PTY subscriber with the router.
/// Called from agent spawn site (caller holds L1/L2 — safe, not router thread).
pub fn register_agent(name: &str, rx: crossbeam_channel::Receiver<Vec<u8>>) {
    if let Some(tx) = REGISTRATION_TX.get() {
        let _ = tx.try_send(AgentSubscription {
            name: name.to_string(),
            rx,
        });
    }
}

/// Per-agent accumulator state.
struct AgentBuffer {
    rx: crossbeam_channel::Receiver<Vec<u8>>,
    buffer: String,
    active: bool,
    last_output_at: Instant,
    input_id: Option<u64>,
}

/// Spawn the router observer thread.
///
/// // fire-and-forget: router thread runs for the daemon process lifetime.
/// // Terminates implicitly on process exit. No graceful-stop needed because
/// // the thread is read-only (subscriber channel consumer + heartbeat_pair
/// // leaf writes). Same shutdown contract as supervisor (see supervisor.rs L58).
pub fn spawn(home: PathBuf, _registry: AgentRegistry) {
    let (tx, rx) = crossbeam_channel::bounded::<AgentSubscription>(64);
    REGISTRATION_TX.set(tx).ok();
    let _ = std::thread::Builder::new()
        .name("router".into())
        .spawn(move || run_loop(home, rx));
}

fn run_loop(home: PathBuf, reg_rx: crossbeam_channel::Receiver<AgentSubscription>) {
    let _census = crate::thread_census::register("router");
    crate::sync_audit::mark_router_thread();

    let mut buffers: HashMap<String, AgentBuffer> = HashMap::new();

    loop {
        // Accept new agent registrations (non-blocking).
        while let Ok(sub) = reg_rx.try_recv() {
            buffers.insert(
                sub.name,
                AgentBuffer {
                    rx: sub.rx,
                    buffer: String::new(),
                    active: false,
                    last_output_at: Instant::now(),
                    input_id: None,
                },
            );
        }

        // Process PTY events from all subscribed agents.
        let mut had_activity = false;
        // CR-2026-06-14 #39: senders that disconnected during the drain below,
        // collected here so the post-loop `buffers.retain` can be a PURE liveness
        // probe (see its comment) rather than a second, ungated try_recv.
        let mut disconnected: Vec<String> = Vec::new();
        for (name, buf) in buffers.iter_mut() {
            loop {
                match buf.rx.try_recv() {
                    Ok(data) => {
                        had_activity = true;
                        buf.last_output_at = Instant::now();

                        let pair = crate::daemon::heartbeat_pair::snapshot_for(name);
                        if pair.reply_to_channel.is_some() {
                            buf.active = true;
                            buf.input_id = pair.reply_to_input_id;
                            let text = String::from_utf8_lossy(&data);
                            buf.buffer.push_str(&text);
                            apply_mirror_cap(&mut buf.buffer);
                        } else {
                            buf.active = false;
                            buf.buffer.clear();
                        }
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        disconnected.push(name.clone());
                        break;
                    }
                }
            }

            // End-of-turn fallback: silence timeout.
            if buf.active
                && !buf.buffer.is_empty()
                && buf.last_output_at.elapsed() > SILENCE_TIMEOUT
            {
                try_dispatch_mirror(&home, name, buf);
            }
        }

        // CR-2026-06-14 #39 (correctness): drop buffers whose sender disconnected.
        // PURE liveness probe — no second try_recv, no payload consumption. The
        // prior version's `Ok(data) => buf.buffer.push_str(&text)` arm pushed
        // bytes consumed here into the mirror buffer UNGATED: it ignored the
        // `reply_to_channel`/`heartbeat_pair` gate (mixing stray bytes in while
        // mirroring was inactive) and skipped `apply_mirror_cap` (an oversized
        // chunk bypassed the 2*MAX_MIRROR_LEN cap). All consume + gating now
        // happens once, in the gated drain loop above; a chunk that races in
        // after the drain is picked up next iteration through that same path.
        buffers.retain(|name, _| !disconnected.contains(name));

        if !had_activity {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Attempt to dispatch accumulated mirror text to the originating channel.
fn try_dispatch_mirror(home: &std::path::Path, name: &str, buf: &mut AgentBuffer) {
    // #2090 mode-1: when the transcript-tail progress mirror is active it OWNS
    // the origin-channel relay; suppress this legacy PTY-buffer mirror to avoid
    // double-sending the same turn's output. (Default progress_mode 0 → no-op.)
    if crate::runtime_config::get().progress_mode == 1 {
        buf.buffer.clear();
        buf.active = false;
        return;
    }
    let pair = crate::daemon::heartbeat_pair::snapshot_for(name);

    if pair.mirror_dispatched_for_turn {
        buf.buffer.clear();
        buf.active = false;
        return;
    }
    if pair.mirror_skip_until_next_turn {
        buf.buffer.clear();
        buf.active = false;
        return;
    }
    if pair.reply_to_channel.is_none() {
        buf.buffer.clear();
        buf.active = false;
        return;
    }
    if let (Some(input_id), Some(last_id)) = (buf.input_id, pair.last_mirror_event_id) {
        if input_id <= last_id {
            buf.buffer.clear();
            buf.active = false;
            return;
        }
    }

    let text = strip_ansi_simple(&buf.buffer);
    let text = text.trim();
    if text.is_empty() || text.len() < 10 {
        buf.buffer.clear();
        buf.active = false;
        return;
    }

    let mirror_text = if text.len() > MAX_MIRROR_LEN {
        let mut end = MAX_MIRROR_LEN;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    } else {
        text
    };

    // #1102 fix: prefer-chain — lookup_channel_by_name first, active_channel fallback.
    // pair.reply_to_channel is verified Some at L148 (early return if None).
    let ch = pair
        .reply_to_channel
        .as_deref()
        .and_then(crate::channel::lookup_channel_by_name)
        .or_else(crate::channel::active_channel);

    if let Some(ch) = ch {
        let result = ch.send_from_agent(
            name,
            crate::channel::AgentOutboundOp::Reply {
                text: mirror_text.to_string(),
            },
        );
        if let Err(e) = result {
            tracing::warn!(agent = %name, error = %e, "mirror dispatch failed");
        } else {
            tracing::debug!(agent = %name, len = mirror_text.len(), "mirror dispatched");
        }
    }

    crate::daemon::heartbeat_pair::update_with(name, |p| {
        p.mirror_dispatched_for_turn = true;
        if let Some(id) = buf.input_id {
            p.last_mirror_event_id = Some(id);
        }
        p.reply_to_channel = None;
        p.reply_to_input_id = None;
    });
    // #1665 reply-ledger: a mirror dispatch IS a closure (the user got the
    // agent's output) — clear the audited turn without warning.
    crate::reply_ledger::clear_turn(name);

    buf.buffer.clear();
    buf.active = false;
    crate::event_log::log(
        home,
        "mirror_dispatch",
        name,
        "response mirrored to channel",
    );
}

/// Simple ANSI escape sequence stripper.
fn strip_ansi_simple(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_escape = false;
    for ch in s.chars() {
        if in_escape {
            if ch.is_ascii_alphabetic() || ch == '~' {
                in_escape = false;
            }
        } else if ch == '\x1b' {
            in_escape = true;
        } else {
            result.push(ch);
        }
    }
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_escape_sequences() {
        assert_eq!(strip_ansi_simple("\x1b[32mhello\x1b[0m"), "hello");
        assert_eq!(strip_ansi_simple("no escapes"), "no escapes");
    }

    #[test]
    fn silence_timeout_is_3s() {
        assert_eq!(SILENCE_TIMEOUT, Duration::from_secs(3));
    }

    #[test]
    fn max_mirror_len_is_4000() {
        assert_eq!(MAX_MIRROR_LEN, 4000);
    }

    #[test]
    fn mirror_drain_start_never_splits_multibyte_char() {
        // 3-byte chars; buffer well over 2*MAX_MIRROR_LEN so the trim branch runs.
        let s: String = "界".repeat(MAX_MIRROR_LEN); // 12000 bytes
        let start = mirror_drain_start(&s);
        // Raw byte target 12000-4000=8000 is NOT a char boundary (8000 % 3 != 0);
        // the fix must snap it forward to one.
        assert!(
            s.is_char_boundary(start),
            "drain start must be a char boundary"
        );
        let mut owned = s.clone();
        owned.drain(..start); // must not panic (regression: assertion failed is_char_boundary)
        assert!(owned.len() <= MAX_MIRROR_LEN + 3);
    }

    #[test]
    fn mirror_drain_start_is_zero_when_short() {
        let s = "界界界".to_string();
        assert_eq!(mirror_drain_start(&s), 0);
    }

    #[test]
    fn run_loop_cap_drain_never_panics_on_oversized_cjk() {
        // §3.9 real-entry regression for #2084/#2085: drive the run-loop
        // per-event consume+cap path (router.rs L113-117) — repeated `push_str`
        // of 3-byte CJK well past 2*MAX_MIRROR_LEN, capping after each push
        // exactly as the run-loop does. The original panic was
        // `buf.drain(..n)` on a non-char-boundary `n` (12000-byte buffer →
        // raw target 8000, and 8000 % 3 != 0). `apply_mirror_cap` must never
        // panic and must hold the buffer at or under 2*MAX_MIRROR_LEN.
        //
        // Distinct from the existing pins: `mirror_drain_start_never_splits_*`
        // exercises only the index helper + a manual drain;
        // `mirror_truncation_safe_on_multibyte_utf8` drives the
        // `try_dispatch_mirror` slice path (:184). Neither calls this drain.
        //
        // RED proof: reverting `mirror_drain_start` to the raw byte index
        // (`buf.len().saturating_sub(MAX_MIRROR_LEN)` without the
        // char-boundary while-snap) makes the first `apply_mirror_cap` below
        // panic ("byte index … is not a char boundary").
        let mut buf = String::new();
        for _ in 0..10 {
            buf.push_str(&"界".repeat(MAX_MIRROR_LEN)); // 12000 bytes/push (> 2*cap)
            apply_mirror_cap(&mut buf); // must not panic on the drain
            assert!(
                buf.len() <= MAX_MIRROR_LEN * 2,
                "cap must hold buffer <= 2*MAX_MIRROR_LEN, got {}",
                buf.len()
            );
        }
        // Drain never split a char → buffer ends on a valid char boundary.
        assert!(buf.is_char_boundary(buf.len()));
    }

    // ── #1102 prefer-chain tests ──────────────────────────────────────

    use crate::channel::{
        AgentOutboundOp, BindingOpts, BindingRef, Channel, ChannelCapabilities, ChannelError,
        ChannelEvent, MsgRef, OutMsg,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct RecordingMockChannel {
        kind_str: &'static str,
        caps: ChannelCapabilities,
        send_count: AtomicUsize,
    }

    impl RecordingMockChannel {
        fn arc(kind: &'static str) -> Arc<Self> {
            Arc::new(Self {
                kind_str: kind,
                caps: ChannelCapabilities::default(),
                send_count: AtomicUsize::new(0),
            })
        }
        fn count(&self) -> usize {
            self.send_count.load(Ordering::Relaxed)
        }
    }

    impl Channel for RecordingMockChannel {
        fn kind(&self) -> &'static str {
            self.kind_str
        }
        fn caps(&self) -> &ChannelCapabilities {
            &self.caps
        }
        fn poll_event(&self) -> Option<ChannelEvent> {
            None
        }
        fn send(&self, _: &BindingRef, _: OutMsg) -> anyhow::Result<MsgRef> {
            anyhow::bail!("unused")
        }
        fn edit(&self, _: &MsgRef, _: OutMsg) -> anyhow::Result<()> {
            Ok(())
        }
        fn delete(&self, _: &MsgRef) -> anyhow::Result<()> {
            Ok(())
        }
        fn create_binding(&self, _: &str, _: BindingOpts) -> anyhow::Result<BindingRef> {
            anyhow::bail!("unused")
        }
        fn remove_binding(&self, _: &BindingRef) -> anyhow::Result<()> {
            Ok(())
        }
        fn has_binding(&self, _: &str) -> bool {
            false
        }
        fn record_binding(&self, _: &str, _: BindingRef, _: String) {}
        fn take_binding(&self, _: &str) -> Option<BindingRef> {
            None
        }
        fn attach_registry(&self, _: crate::agent::AgentRegistry) {}
        fn send_from_agent(&self, _: &str, _: AgentOutboundOp) -> Result<MsgRef, ChannelError> {
            self.send_count.fetch_add(1, Ordering::Relaxed);
            Ok(MsgRef {
                binding: BindingRef::new(self.kind_str, None, ()),
                id: "mirror-msg".into(),
            })
        }
    }

    fn registry_guard() -> parking_lot::MutexGuard<'static, ()> {
        static G: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
        G.lock()
    }

    fn make_buffer(text: &str) -> AgentBuffer {
        let (_tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(1);
        AgentBuffer {
            rx,
            buffer: text.to_string(),
            active: true,
            last_output_at: Instant::now() - Duration::from_secs(10),
            input_id: Some(42),
        }
    }

    #[test]
    fn mirror_dispatch_uses_named_channel_in_multi_channel_fleet() {
        let _g = registry_guard();
        crate::channel::reset_active_channel_for_test();

        let tg = RecordingMockChannel::arc("telegram");
        let dc = RecordingMockChannel::arc("discord");
        crate::channel::register_active_channel(tg.clone());
        crate::channel::register_active_channel(dc.clone());

        assert!(
            crate::channel::active_channel().is_none(),
            "active_channel must return None with 2 channels"
        );

        let agent = "test_mirror_multichan";
        crate::daemon::heartbeat_pair::update_with(agent, |p| {
            p.reply_to_channel = Some("telegram".into());
            p.reply_to_input_id = Some(42);
            p.mirror_dispatched_for_turn = false;
            p.mirror_skip_until_next_turn = false;
            p.last_mirror_event_id = None;
        });

        let home = std::env::temp_dir().join(format!("agend-router-1102-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();

        let mut buf = make_buffer("This is mirror text that should be dispatched");
        try_dispatch_mirror(&home, agent, &mut buf);

        assert_eq!(tg.count(), 1, "telegram channel must receive the mirror");
        assert_eq!(dc.count(), 0, "discord channel must NOT receive the mirror");
        assert!(
            buf.buffer.is_empty(),
            "buffer must be cleared after dispatch"
        );
        assert!(!buf.active, "active must be false after dispatch");

        let pair = crate::daemon::heartbeat_pair::snapshot_for(agent);
        assert!(pair.mirror_dispatched_for_turn);
        assert!(pair.reply_to_channel.is_none());

        crate::channel::reset_active_channel_for_test();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn mirror_truncation_safe_on_multibyte_utf8() {
        let _g = registry_guard();
        crate::channel::reset_active_channel_for_test();

        let tg = RecordingMockChannel::arc("telegram");
        crate::channel::register_active_channel(tg.clone());

        let agent = "test_mirror_utf8";
        crate::daemon::heartbeat_pair::update_with(agent, |p| {
            p.reply_to_channel = Some("telegram".into());
            p.reply_to_input_id = Some(42);
            p.mirror_dispatched_for_turn = false;
            p.mirror_skip_until_next_turn = false;
            p.last_mirror_event_id = None;
        });

        let home = std::env::temp_dir().join(format!("agend-router-utf8-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();

        // Build a string that has multibyte chars near MAX_MIRROR_LEN boundary
        let prefix = "x".repeat(MAX_MIRROR_LEN - 2);
        let text = format!("{prefix}日本語"); // 3-byte chars right at the boundary
        let mut buf = make_buffer(&text);
        // Must not panic
        try_dispatch_mirror(&home, agent, &mut buf);
        assert_eq!(tg.count(), 1);

        crate::channel::reset_active_channel_for_test();
        std::fs::remove_dir_all(&home).ok();
    }

    // CR-2026-06-14 #39: `retain_preserves_received_data` was REMOVED. It built a
    // COPY of the trailing `buffers.retain` closure and asserted its Ok-arm pushed
    // consumed bytes into `buf.buffer` — i.e. it PINNED the ungated-push bug this
    // fix removes. The post-fix `buffers.retain` is a pure liveness probe (drops
    // disconnected buffers, no payload push); the source-scan repro
    // `router_retain_probe_does_not_ungated_push_into_mirror_buffer_daemon_supervisor`
    // (tests/) now guards that invariant. Gated consume is covered by
    // `apply_mirror_cap` tests + the run_loop drain path.

    #[test]
    fn mirror_dispatch_falls_back_to_active_channel_when_lookup_fails() {
        let _g = registry_guard();
        crate::channel::reset_active_channel_for_test();

        let tg = RecordingMockChannel::arc("telegram");
        crate::channel::register_active_channel(tg.clone());

        assert!(
            crate::channel::active_channel().is_some(),
            "single channel → active_channel must return Some"
        );

        let agent = "test_mirror_fallback";
        crate::daemon::heartbeat_pair::update_with(agent, |p| {
            p.reply_to_channel = Some("nonexistent".into());
            p.reply_to_input_id = Some(99);
            p.mirror_dispatched_for_turn = false;
            p.mirror_skip_until_next_turn = false;
            p.last_mirror_event_id = None;
        });

        let home =
            std::env::temp_dir().join(format!("agend-router-1102-fb-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();

        let mut buf = make_buffer("Fallback mirror text for single channel fleet");
        try_dispatch_mirror(&home, agent, &mut buf);

        assert_eq!(tg.count(), 1, "fallback to active_channel must work");
        assert!(buf.buffer.is_empty());

        crate::channel::reset_active_channel_for_test();
        std::fs::remove_dir_all(&home).ok();
    }
}
