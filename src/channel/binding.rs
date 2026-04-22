//! Opaque binding references.
//!
//! A `BindingRef` identifies a "place" (Telegram topic, Discord channel,
//! Slack thread) that an agent is paired to. Core code **never** looks
//! inside — it hands the ref back to the owning `Channel` impl when
//! sending / editing / deleting.
//!
//! The inner platform payload is type-erased via `Arc<dyn Any + Send + Sync>`
//! so adapters can attach whatever shape they need (e.g. Telegram stores
//! `{ chat_id: i64, topic_id: Option<i32> }`; Discord stores
//! `{ guild_id, channel_id }`).

use std::any::Any;
use std::fmt;
use std::sync::Arc;

/// Opaque reference to a channel binding.
///
/// `Arc` makes clones cheap (cloning is O(1) — the payload stays shared).
/// The struct has no public accessor for `payload` — adapters retrieve it
/// via the crate-private [`Self::downcast`] method.
#[derive(Clone)]
pub struct BindingRef {
    kind: &'static str,
    display_tag: Option<String>,
    payload: Arc<dyn Any + Send + Sync>,
}

impl BindingRef {
    /// Construct a new binding ref. `kind` should match the owning
    /// channel's `Channel::kind()`. `display_tag` is a human-readable
    /// tag shown in the TUI / logs (e.g. "TG#229").
    pub fn new<T: Any + Send + Sync + 'static>(
        kind: &'static str,
        display_tag: Option<String>,
        payload: T,
    ) -> Self {
        Self {
            kind,
            display_tag,
            payload: Arc::new(payload),
        }
    }

    /// Kind discriminator, matches the owning `Channel::kind()`.
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// Optional human-readable tag for the TUI / logs.
    ///
    /// Adopted up-front per spike report §3.5: without this accessor,
    /// call sites that render an "is-bound" indicator (today
    /// `src/render.rs` reads `topic_id` directly) would re-leak the
    /// concrete platform type through a getter. Exposing only a string
    /// keeps the opacity promise intact.
    pub fn display_tag(&self) -> Option<&str> {
        self.display_tag.as_deref()
    }

    /// Adapter-side downcast to the underlying platform payload. Returns
    /// `None` if the payload is not the requested type — this lets the
    /// adapter assert its own shape without panicking on a misrouted ref.
    pub fn downcast<T: Any + Send + Sync + 'static>(&self) -> Option<&T> {
        self.payload.downcast_ref::<T>()
    }
}

impl fmt::Debug for BindingRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BindingRef")
            .field("kind", &self.kind)
            .field("display_tag", &self.display_tag)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct TgPayload {
        chat_id: i64,
        topic_id: Option<i32>,
    }

    #[test]
    fn display_tag_round_trips() {
        let b = BindingRef::new(
            "telegram",
            Some("TG#229".to_string()),
            TgPayload {
                chat_id: -100,
                topic_id: Some(229),
            },
        );
        assert_eq!(b.kind(), "telegram");
        assert_eq!(b.display_tag(), Some("TG#229"));
    }

    #[test]
    fn display_tag_none_when_omitted() {
        let b = BindingRef::new(
            "telegram",
            None,
            TgPayload {
                chat_id: -100,
                topic_id: None,
            },
        );
        assert_eq!(b.display_tag(), None);
    }

    #[test]
    fn downcast_returns_payload_for_matching_type() {
        let b = BindingRef::new(
            "telegram",
            None,
            TgPayload {
                chat_id: -42,
                topic_id: Some(7),
            },
        );
        let tg = b.downcast::<TgPayload>().expect("downcast");
        assert_eq!(tg.chat_id, -42);
        assert_eq!(tg.topic_id, Some(7));
    }

    #[test]
    fn downcast_returns_none_for_mismatched_type() {
        let b = BindingRef::new(
            "telegram",
            None,
            TgPayload {
                chat_id: 0,
                topic_id: None,
            },
        );
        // Asking for a different type must not panic.
        assert!(b.downcast::<String>().is_none());
    }

    #[test]
    fn clone_shares_payload() {
        let b1 = BindingRef::new(
            "telegram",
            Some("a".to_string()),
            TgPayload {
                chat_id: 1,
                topic_id: None,
            },
        );
        let b2 = b1.clone();
        // Both clones should downcast to the same payload.
        let p1 = b1.downcast::<TgPayload>().expect("b1 downcast");
        let p2 = b2.downcast::<TgPayload>().expect("b2 downcast");
        assert_eq!(p1, p2);
    }
}
