//! Bearer tokens for [`Endpoint::Vertex`]: Google Application Default
//! Credentials through `gcp_auth` (`GOOGLE_APPLICATION_CREDENTIALS`, the
//! `gcloud auth application-default login` file, the metadata server on
//! GCE/GKE/Cloud Run, then `gcloud auth print-access-token`), behind
//! [`TokenSource`] so a test can inject a token and assert the
//! `Authorization: Bearer` header against wiremock without a Google account.
//!
//! [`Endpoint::Vertex`]: crate::Endpoint::Vertex

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use judge_llm::{ApiKey, LlmError};

/// The OAuth scope Vertex AI wants.
pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// The `door` an [`LlmError::Auth`] from the credential chain names.
const PLATFORM: &str = "gcp";

/// Where the bearer token comes from.
#[async_trait]
pub trait TokenSource: Send + Sync + fmt::Debug {
    /// A currently valid access token (a provider refreshes it itself).
    ///
    /// # Errors
    /// [`LlmError::Auth`] when the source has none.
    async fn token(&self) -> Result<ApiKey, LlmError>;
}

/// Application Default Credentials, discovered lazily on first use so that
/// building an [`crate::Anthropic`] never touches the network or the disk.
#[derive(Default)]
pub struct Adc {
    provider: tokio::sync::OnceCell<Arc<dyn gcp_auth::TokenProvider>>,
}

impl Adc {
    /// The chain, not yet resolved.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl fmt::Debug for Adc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Adc")
    }
}

#[async_trait]
impl TokenSource for Adc {
    async fn token(&self) -> Result<ApiKey, LlmError> {
        let auth = |e: gcp_auth::Error| LlmError::Auth {
            door: PLATFORM,
            message: e.to_string(),
        };
        let provider = self
            .provider
            .get_or_try_init(|| async { gcp_auth::provider().await.map_err(auth) })
            .await?;
        let token = provider.token(&[SCOPE]).await.map_err(auth)?;
        Ok(ApiKey::from(token.as_str()))
    }
}

/// A fixed token, for tests against a mock server. Its `Debug` is redacted.
#[derive(Clone, Debug)]
pub struct StaticToken(ApiKey);

impl StaticToken {
    /// Wrap `token`.
    #[must_use]
    pub fn new(token: impl Into<ApiKey>) -> Self {
        Self(token.into())
    }
}

#[async_trait]
impl TokenSource for StaticToken {
    async fn token(&self) -> Result<ApiKey, LlmError> {
        Ok(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_static_token_is_handed_back_and_never_printed() -> Result<(), LlmError> {
        let t = StaticToken::new("ya29.secret");
        assert_eq!(t.token().await?.expose(), "ya29.secret");
        assert!(!format!("{t:?}").contains("secret"));
        assert_eq!(format!("{:?}", Adc::new()), "Adc");
        Ok(())
    }
}
