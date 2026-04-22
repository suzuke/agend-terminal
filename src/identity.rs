//! Sender identity as a non-empty newtype. Handlers that require identity
//! take `&Sender`; handlers that tolerate anonymous (standalone) mode take
//! `Option<&Sender>`.

use std::fmt;

/// A validated, non-empty instance identifier used as the "from" stamp on
/// cross-instance messages.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Sender(String);

impl Sender {
    /// Construct a `Sender`. Returns `None` if the input is empty.
    pub fn new(s: impl Into<String>) -> Option<Self> {
        let s = s.into();
        (!s.is_empty()).then_some(Self(s))
    }

    /// Read the sender identity from `AGEND_INSTANCE_NAME`. Returns `None`
    /// if the env var is unset or empty (standalone / unnamed mode).
    pub fn from_env() -> Option<Self> {
        std::env::var("AGEND_INSTANCE_NAME")
            .ok()
            .and_then(Self::new)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<&str> for Sender {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty() {
        assert!(Sender::new("").is_none());
        assert!(Sender::new(String::new()).is_none());
    }

    #[test]
    fn accepts_non_empty() {
        let s = Sender::new("alice").expect("non-empty");
        assert_eq!(s.as_str(), "alice");
        assert_eq!(s.to_string(), "alice");
    }

    #[test]
    fn env_set_returns_some() {
        // SAFETY: env mutation in tests is racy; serialize via a mutex.
        // `set_var`/`remove_var` are unsafe in 2024 because they mutate
        // process-global state.
        let _g = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("AGEND_INSTANCE_NAME", "dev-1");
        }
        let got = Sender::from_env().expect("set");
        assert_eq!(got.as_str(), "dev-1");
        unsafe {
            std::env::remove_var("AGEND_INSTANCE_NAME");
        }
    }

    fn env_lock() -> &'static std::sync::Mutex<()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn eq_against_str() {
        let s = Sender::new("bob").unwrap();
        assert_eq!(s, "bob");
    }
}
