//! The operator's alert webhook (`JUDGE_ALERT_WEBHOOK`): where a long-running
//! process reports what nobody would otherwise notice until they read the log.
//! [`crate::budget`] posts a tripped spend cap here and [`crate::jobs`] a
//! scheduled refresh that failed, recovered, or left rows unembedded.
//!
//! An alert is best effort: a webhook that refuses, times out or cannot be
//! reached is logged and never stops the caller. The URL is a credential, so
//! it is never logged or printed: only its host is.

use std::time::Duration;

/// `JUDGE_ALERT_WEBHOOK`.
pub const ALERT_WEBHOOK_ENV: &str = "JUDGE_ALERT_WEBHOOK";
/// How long the alert webhook gets to answer.
pub const ALERT_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the operator is told: an `https` webhook that takes a JSON body.
/// Redacted in `Debug`, because a webhook URL is a credential: anyone holding
/// it can post to the channel.
#[derive(Clone, PartialEq, Eq)]
pub struct AlertWebhook(url::Url);

impl AlertWebhook {
    /// Parse the variable's value.
    ///
    /// # Errors
    /// Anything that is not an absolute `https` URL with a host.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match url::Url::parse(raw.trim()) {
            Ok(u) if u.scheme() == "https" && u.host_str().is_some() => Ok(Self(u)),
            _ => Err("an https:// webhook URL".to_owned()),
        }
    }

    /// The host, which is safe to log.
    #[must_use]
    pub fn host(&self) -> &str {
        self.0.host_str().unwrap_or_default()
    }
}

impl std::fmt::Debug for AlertWebhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AlertWebhook(https://{}/<redacted>)", self.host())
    }
}

/// The client alerts are sent with: bounded by [`ALERT_TIMEOUT`], so a
/// webhook that accepts the connection and never answers cannot stall the
/// loop that sends them.
#[must_use]
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(ALERT_TIMEOUT)
        .build()
        .unwrap_or_default()
}

/// Post `text` to the webhook; `what` names the alert in the log lines
/// (`spend cap alert sent`). The body carries both `content` (Discord) and
/// `text` (Slack and its imitators); each ignores the other's key.
pub async fn post(client: &reqwest::Client, hook: &AlertWebhook, what: &str, text: &str) {
    let body = serde_json::json!({ "content": text, "text": text });
    match client.post(hook.0.clone()).json(&body).send().await {
        Ok(r) if r.status().is_success() => {
            tracing::info!(host = hook.host(), "{what} sent");
        }
        Ok(r) => {
            tracing::warn!(host = hook.host(), status = %r.status(), "{what} refused");
        }
        // `without_url`: the URL is the credential.
        Err(e) => {
            tracing::warn!(host = hook.host(), error = %e.without_url(), "{what} failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_webhook_must_be_https_and_never_prints_its_path() -> Result<(), String> {
        let hook = AlertWebhook::parse("https://discord.com/api/webhooks/1/secret-token")?;
        let shown = format!("{hook:?}");
        assert!(
            shown.contains("discord.com") && !shown.contains("secret"),
            "{shown}"
        );
        for bad in ["http://example.com/hook", "discord.com/api", "", "https://"] {
            assert!(AlertWebhook::parse(bad).is_err(), "{bad}");
        }
        Ok(())
    }
}
