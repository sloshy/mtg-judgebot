//! Whether this process answers questions yet: closed for maintenance while
//! a major upgrade waits for the previous release to stop and migrates the
//! schema, open otherwise.
//!
//! During a major upgrade the new process starts its roles at once, closed
//! ([`Availability::upgrading`]): Discord, the web app and `/mcp` are
//! reachable and answer each question with [`MESSAGE`] and a way to retry,
//! touching no table. It takes the exclusive schema hold
//! ([`crate::lease::SchemaHold`]) once every process of the previous release
//! has exited, migrates, and opens ([`Availability::set_open`]). Any other
//! start migrates first and is open from the beginning.

use std::{sync::Arc, time::Duration};

use tokio::sync::watch;

/// What a question gets while the process is closed.
pub const MESSAGE: &str = "Judgebot is being upgraded and will be back in a minute or two. \
Your question was not answered; please try again shortly.";

/// How long a client is told to wait before retrying (`Retry-After`).
pub const RETRY_AFTER: Duration = Duration::from_secs(30);

/// Open or closed for maintenance; cheap to clone, shared by every role of
/// the process.
#[derive(Clone, Debug)]
pub struct Availability(Arc<watch::Sender<bool>>);

impl Availability {
    /// Open from the start.
    #[must_use]
    pub fn open() -> Self {
        Self(Arc::new(watch::Sender::new(true)))
    }

    /// Closed until [`Self::set_open`].
    #[must_use]
    pub fn upgrading() -> Self {
        Self(Arc::new(watch::Sender::new(false)))
    }

    /// Whether questions are answered.
    #[must_use]
    pub fn is_open(&self) -> bool {
        *self.0.borrow()
    }

    /// Open: the migration is done.
    pub fn set_open(&self) {
        self.0.send_replace(true);
    }

    /// Wait until open.
    pub async fn opened(&self) {
        let mut rx = self.0.subscribe();
        // The sender lives in `self`, so the wait cannot end in an error.
        let _ = rx.wait_for(|open| *open).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn closed_until_opened_and_open_stays_open() {
        let a = Availability::upgrading();
        assert!(!a.is_open());
        let waiting = tokio::spawn({
            let a = a.clone();
            async move { a.opened().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        a.set_open();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), waiting)
                .await
                .is_ok()
        );
        assert!(a.is_open());
        Availability::open().opened().await;
    }
}
