//! Everything a chat backend, or the spend cap around it, can fail with.

use reqwest::StatusCode;

use crate::Billed;

/// Errors from [`crate::ChatModel::complete`] and [`crate::Metered`].
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// Connection / timeout / TLS.
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    /// Non-2xx status with the provider's error body.
    #[error("api {status}: {kind}: {message}")]
    Api {
        /// HTTP status.
        status: StatusCode,
        /// The provider's error type, e.g. `rate_limit_error`.
        kind: String,
        /// The provider's message.
        message: String,
    },
    /// A 2xx body that does not decode as a response. `billed` is its
    /// leniently parsed usage, if that much parsed: the provider charged for
    /// it, so the spend cap does too.
    #[error("bad response body: {source}")]
    Decode {
        /// The decode failure.
        #[source]
        source: serde_json::Error,
        /// What the body could still be billed as.
        billed: Option<Billed>,
    },
    /// The request cannot be expressed for this backend.
    #[error("request not expressible for this backend: {0}")]
    Request(String),
    /// An [`crate::AssistantTurn`] produced by one backend was replayed to
    /// another. The turn is opaque, so the backend does not guess.
    #[error("assistant turn from backend {found:?} replayed to backend {expected:?}")]
    ForeignTurn {
        /// The backend that received it.
        expected: &'static str,
        /// The backend that produced it.
        found: &'static str,
    },
    /// A credential environment variable is unset.
    #[error("{var} is not set")]
    MissingApiKey {
        /// The variable.
        var: &'static str,
    },
    /// Cumulative estimated spend reached the cap; the request was not sent.
    #[error("spend cap exceeded: spent ${spent:.4} of ${cap:.2} cap; request not sent")]
    SpendCapExceeded {
        /// Estimated USD spent so far across everything sharing the meter.
        spent: f64,
        /// The cap in USD.
        cap: f64,
    },
    /// A spend cap is not a finite non-negative number. `setting` names
    /// where it came from (`JUDGE_MAX_USD`, or "spend cap" for a value handed
    /// to [`crate::SpendMeter::with_max_spend_usd`], as `--max-usd` does).
    #[error("{setting} is not a finite non-negative number: {value:?}")]
    BadMaxSpend {
        /// The environment variable or setting that carried the value.
        setting: &'static str,
        /// The offending value.
        value: String,
    },
    /// The price table has no entry for this model and the provider has no
    /// erring-high rule, so the spend cap could not reserve anything.
    #[error("no price for {provider}/{model}; the spend cap cannot estimate its cost")]
    Unpriced {
        /// Provider key.
        provider: String,
        /// Model id.
        model: String,
    },
}

impl LlmError {
    /// 408, 409, 429, 529 and 5xx are retried, as in the official SDKs, and
    /// so are connection-level transport failures.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            LlmError::Transport(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            LlmError::Api { status, .. } => matches!(status.as_u16(), 408 | 409 | 429 | 529) || status.is_server_error(),
            LlmError::Decode { .. }
            | LlmError::Request(_)
            | LlmError::ForeignTurn { .. }
            | LlmError::MissingApiKey { .. }
            | LlmError::SpendCapExceeded { .. }
            | LlmError::BadMaxSpend { .. }
            | LlmError::Unpriced { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_statuses() {
        let api = |code: u16| LlmError::Api {
            status: StatusCode::from_u16(code).unwrap_or(StatusCode::OK),
            kind: String::new(),
            message: String::new(),
        };
        for code in [408, 409, 429, 500, 502, 529] {
            assert!(api(code).is_retryable(), "{code}");
        }
        for code in [400, 401, 403, 404, 413] {
            assert!(!api(code).is_retryable(), "{code}");
        }
        assert!(!LlmError::MissingApiKey { var: "X" }.is_retryable());
        assert!(!LlmError::SpendCapExceeded { spent: 1.0, cap: 1.0 }.is_retryable());
        assert!(!LlmError::ForeignTurn { expected: "a", found: "b" }.is_retryable());
    }
}
