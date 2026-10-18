//! The database side of a [`Toolbox`](crate::Toolbox), reachable only
//! through a passed release check.
//!
//! Every port that reads or writes the instance's database lives in [`Db`],
//! whose fields are private to this module. An operation gets them through
//! [`Db::check`], which returns a [`Checked`] only when this tool's release can
//! work with the database's ([`crate::release`]). So an operation that skips
//! the check does not compile. The one unchecked read is the data's freshness
//! for `about`, which says where the source is whatever the release.

use std::sync::Arc;

use judge_bot::{db::PgLibrary, session::Sessions};
use judge_core::{CallStore, Freshness, Resolver, Retriever};

use crate::{
    Pipeline,
    release::{Notice, ReleaseCheck},
};

/// The ports over the database, and the check that guards them.
pub(crate) struct Db {
    sessions: Sessions,
    library: PgLibrary,
    resolver: Arc<dyn Resolver>,
    retriever: Arc<dyn Retriever>,
    calls: Arc<dyn CallStore>,
    pipeline: Option<Pipeline>,
    /// `None` inside the instance, whose release is its own.
    release: Option<ReleaseCheck>,
}

/// Access to [`Db`] after a passed check.
pub(crate) struct Checked<'a>(&'a Db);

/// What [`Db::new`] wires together.
pub(crate) struct Parts {
    pub(crate) sessions: Sessions,
    pub(crate) library: PgLibrary,
    pub(crate) resolver: Arc<dyn Resolver>,
    pub(crate) retriever: Arc<dyn Retriever>,
    pub(crate) calls: Arc<dyn CallStore>,
    pub(crate) pipeline: Option<Pipeline>,
    pub(crate) release: Option<ReleaseCheck>,
}

impl Db {
    pub(crate) fn new(p: Parts) -> Self {
        Self {
            sessions: p.sessions,
            library: p.library,
            resolver: p.resolver,
            retriever: p.retriever,
            calls: p.calls,
            pipeline: p.pipeline,
            release: p.release,
        }
    }

    /// The ports, when this tool's release can work with the database's;
    /// otherwise the reason, saying what to update.
    pub(crate) async fn check(&self) -> anyhow::Result<Checked<'_>> {
        match self.notice().await {
            Some(Notice::Stop(why)) => Err(anyhow::anyhow!(why)),
            Some(Notice::Info(_)) | None => Ok(Checked(self)),
        }
    }

    /// What this tool says about the instance's release; `None` inside the
    /// instance.
    pub(crate) async fn notice(&self) -> Option<Notice> {
        match &self.release {
            Some(check) => check.notice().await,
            None => None,
        }
    }

    /// [`Self::notice`] as last read.
    pub(crate) fn notice_cached(&self) -> Option<Notice> {
        self.release.as_ref().and_then(ReleaseCheck::cached)
    }

    /// The data's freshness, for `about`: the one read without a check.
    pub(crate) async fn freshness(&self) -> Option<Freshness> {
        self.library.freshness().await
    }

    /// Whether the built-in pipeline can run.
    pub(crate) const fn has_pipeline(&self) -> bool {
        self.pipeline.is_some()
    }
}

impl Checked<'_> {
    pub(crate) const fn sessions(&self) -> &Sessions {
        &self.0.sessions
    }

    pub(crate) const fn library(&self) -> &PgLibrary {
        &self.0.library
    }

    pub(crate) fn resolver(&self) -> &dyn Resolver {
        &*self.0.resolver
    }

    pub(crate) fn retriever(&self) -> &dyn Retriever {
        &*self.0.retriever
    }

    pub(crate) fn calls(&self) -> &dyn CallStore {
        &*self.0.calls
    }

    pub(crate) const fn pipeline(&self) -> Option<&Pipeline> {
        self.0.pipeline.as_ref()
    }
}
