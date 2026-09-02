//! [`ApiKey`]: a credential that cannot reach a log line by accident. Its
//! `Debug` is the literal `<redacted>`, it has no `Display` and no
//! `Serialize`, so the only way to the wire is [`ApiKey::expose`] at the
//! one place a backend adds its auth header.

use std::fmt;

/// An API key. Its `Debug` is redacted so it can never reach a `{:?}` log line.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// The key as sent on the wire.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for ApiKey {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ApiKey {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_key() {
        let k = ApiKey::from("sk-very-secret");
        assert_eq!(format!("{k:?}"), "<redacted>");
        assert_eq!(format!("{:?}", Some(k.clone())), "Some(<redacted>)");
        assert_eq!(k.expose(), "sk-very-secret");
    }
}
