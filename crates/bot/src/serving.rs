//! The composition every serving role of a process builds on: the models
//! behind the one spend meter, the vector space check, the call store and the
//! spend ledger. `judgebot` builds one [`Serving`] whatever roles it opens, so
//! the Discord bot and the HTTP interfaces of one process share a meter, a
//! `Vectors` and a budget period, and each role adds only its own adapter
//! ([`crate::discord::serve`], `judge_api::run`).

use std::sync::Arc;

use anyhow::Result;
use judge_core::{CallStore, Deps};
use sqlx::PgPool;

use crate::{
    Models, budget, build_deps_with,
    config::Config,
    db::{PgCallStore, Vectors},
};

/// The process's shared composition. Made only by [`Serving::start`], which
/// also loads the period's spend, so no role can answer a question before the
/// cap knows what has been spent.
pub struct Serving {
    pool: PgPool,
    judge: Config,
    models: Models,
    vectors: Option<Arc<Vectors>>,
    store: Arc<dyn CallStore>,
    process: &'static str,
}

impl Serving {
    /// Over `models` (built from `judge` before anything connected, one
    /// meter): probe a cloud endpoint's credentials, run the vector space
    /// check once, open the call store and start the spend ledger for
    /// `process` (its name in alerts and the log).
    ///
    /// # Errors
    /// A cloud endpoint whose credential chain is empty, or an embedder that
    /// cannot be built.
    pub async fn start(
        pool: PgPool,
        judge: &Config,
        models: Models,
        process: &'static str,
    ) -> Result<Self> {
        // A cloud endpoint with no credentials fails here, not on the first question.
        judge.probe_auth().await?;
        // The embedder is optional: without one the retriever skips its vector
        // source. One `Vectors` for every adapter in the process: the space
        // check against `embedding_space` runs once and disables them all on
        // a mismatch.
        let vectors = judge.vectors(pool.clone())?;
        // The verdict (on, absent row, mismatch) lands here beside the summary,
        // not in the first request's log.
        if let Some(v) = &vectors {
            v.enabled().await;
        } else {
            tracing::warn!(
                "no embedder configured (VOYAGE_API_KEY or [models.embed]); running without vector search"
            );
        }
        let mut store = PgCallStore::new(pool.clone());
        if let Some(v) = &vectors {
            store = store.with_vectors(Arc::clone(v));
        }
        let store: Arc<dyn CallStore> = Arc::new(store);
        // The period's spend so far is loaded before the first question can
        // arrive, on any role: they all bill to this one meter.
        budget::start(
            pool.clone(),
            models.meter().clone(),
            judge.budget().clone(),
            process,
        )
        .await;
        tracing::info!(source = %judge.source_offer(), "source offer");
        Ok(Self {
            pool,
            judge: judge.clone(),
            models,
            vectors,
            store,
            process,
        })
    }

    /// The process's name, for alerts, the log and its leases.
    #[must_use]
    pub const fn process(&self) -> &'static str {
        self.process
    }

    /// The pool every adapter of the process shares.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The loaded configuration.
    #[must_use]
    pub fn judge(&self) -> &Config {
        &self.judge
    }

    /// The models, billed to the process's one meter.
    #[must_use]
    pub fn models(&self) -> &Models {
        &self.models
    }

    /// The process's one `Vectors`, if an embedder is configured.
    #[must_use]
    pub fn vectors(&self) -> Option<&Arc<Vectors>> {
        self.vectors.as_ref()
    }

    /// The call store (with the vectors, when there are any).
    #[must_use]
    pub fn store(&self) -> Arc<dyn CallStore> {
        Arc::clone(&self.store)
    }

    /// A fresh [`Deps`] over the shared models and vectors. Each role takes
    /// its own: the Discord layer wraps the retriever in its own capture.
    #[must_use]
    pub fn deps(&self) -> Deps {
        build_deps_with(
            self.pool.clone(),
            &self.models,
            self.vectors.clone(),
            &self.judge.deps_config(),
        )
    }
}
