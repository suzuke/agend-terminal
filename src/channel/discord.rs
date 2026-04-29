//! Discord adapter — behind the `discord` feature gate.
//!
//! PR1 scope: gateway scaffold + auth + `ChannelEvent::Connected`.
//! Other trait methods stub `Err(NotSupported)` until PR2-4.

#[cfg(test)]
mod tests {
    use crate::channel::ChannelEvent;

    /// §3.5.10 wire-format fixture: Discord Gateway READY payload
    /// (tests/fixtures/discord-gateway-ready.json) is deserialized via
    /// twilight-model and mapped to `ChannelEvent::Connected`.
    ///
    /// §3.5.11 test-first: this test is committed RED before the
    /// implementation exists. The GREEN commit adds `map_ready_to_connected`.
    #[test]
    fn discord_gateway_ready_emits_connected_event() {
        // The fixture contains the full gateway frame (op + d + s + t).
        // We extract the inner `d` object and deserialize as `Ready`.
        let fixture = include_str!("../../tests/fixtures/discord-gateway-ready.json");
        let frame: serde_json::Value =
            serde_json::from_str(fixture).expect("fixture must parse as JSON");
        let d = frame.get("d").expect("fixture must have 'd' field");
        let ready: twilight_model::gateway::payload::incoming::Ready =
            serde_json::from_value(d.clone()).expect("'d' must parse as Ready");

        let event = super::map_ready_to_connected(&ready);

        match event {
            ChannelEvent::Connected { kind, who } => {
                assert_eq!(kind, "discord");
                assert_eq!(who, "agend-bot");
            }
            other => panic!("expected Connected, got: {other:?}"),
        }
    }

    /// Contract test: DiscordChannel satisfies the registry-side
    /// contract from `src/channel/contract.rs`.
    #[test]
    fn discord_channel_satisfies_contract() {
        let (ch, _rx) = super::DiscordChannel::new_for_test();
        crate::channel::contract::run_registry_contract(ch, super::discord_make_binding);
    }

    /// Caps snapshot: pin the Discord capability matrix so reviewers
    /// can diff against the S5 analysis.
    #[test]
    fn discord_caps_match_s5_analysis() {
        let (ch, _rx) = super::DiscordChannel::new_for_test();
        let caps = crate::channel::Channel::caps(&ch);

        assert!(caps.emits_deletion_events);
        assert!(caps.threads);
        assert!(caps.attachments);
        assert!(caps.react);
        assert!(caps.edit);
        assert!(caps.typing_indicator);
        assert!(caps.receives_edit_events);
        assert_eq!(caps.max_msg_bytes, 2000);
        assert_eq!(caps.markdown, crate::channel::MarkdownDialect::DiscordMd);
        assert_eq!(
            caps.mention_parsing_hint,
            crate::channel::MentionStyle::AtSnowflake
        );
        assert!(!caps.bot_sees_read_receipts);
        assert!(caps.has_native_multi_thread_view.is_none());
        assert!(!caps.ephemeral);
    }

    /// poll_event drains the internal mpsc channel.
    #[test]
    fn poll_event_drains_mpsc() {
        let (ch, tx) = super::DiscordChannel::new_for_test();
        // Empty channel returns None.
        assert!(crate::channel::Channel::poll_event(&ch).is_none());

        // Send a Connected event through the channel.
        tx.send(ChannelEvent::Connected {
            kind: "discord".into(),
            who: "test-bot".into(),
        })
        .expect("send");

        let event = crate::channel::Channel::poll_event(&ch).expect("should have event");
        match event {
            ChannelEvent::Connected { kind, who } => {
                assert_eq!(kind, "discord");
                assert_eq!(who, "test-bot");
            }
            other => panic!("expected Connected, got: {other:?}"),
        }

        // Drained — next poll returns None.
        assert!(crate::channel::Channel::poll_event(&ch).is_none());
    }
}
