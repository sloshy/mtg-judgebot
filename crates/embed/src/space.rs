//! [`Space`]: which vectors these are. Two vectors are comparable only when
//! they came from the same provider kind, the same model and the same width;
//! the database records one such triple and everything that reads or writes
//! a vector column checks against it first.

use std::{fmt, str::FromStr};

use judge_core::Embedder;

/// The kind of service that produced a vector — the wire API, not the
/// operator's name for the provider in `judge.toml` (two operators can call
/// the same Ollama `litellm` and `local`; the vectors are the same either
/// way). Stored as text in `embedding_space.provider`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    /// Voyage AI.
    Voyage,
    /// An OpenAI-compatible `/v1/embeddings` server.
    OpenAi,
}

impl Provider {
    /// The stored form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Provider::Voyage => "voyage",
            Provider::OpenAi => "openai",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A stored provider name this binary does not know.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown embedding provider {0:?} (expected voyage or openai)")]
pub struct UnknownProvider(pub String);

impl FromStr for Provider {
    type Err = UnknownProvider;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "voyage" => Ok(Provider::Voyage),
            "openai" => Ok(Provider::OpenAi),
            other => Err(UnknownProvider(other.to_owned())),
        }
    }
}

/// The identity of a vector space: provider kind, model and width.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Space {
    /// Who produced the vectors.
    pub provider: Provider,
    /// The model, as the provider names it.
    pub model: String,
    /// Vector width; the `vector(N)` of the columns.
    pub dimensions: usize,
}

impl Space {
    /// Whether vectors written by an embedder in `self` may be mixed with
    /// vectors stored in `stored`: exactly when the two are the same space.
    /// Pure, so the retriever, the call store and `ingest embed` cannot
    /// disagree about what "the same" means.
    ///
    /// # Errors
    /// [`SpaceMismatch`] naming both spaces.
    pub fn check(&self, stored: &Space) -> Result<(), SpaceMismatch> {
        if self == stored {
            Ok(())
        } else {
            Err(SpaceMismatch {
                configured: self.clone(),
                stored: stored.clone(),
            })
        }
    }
}

impl fmt::Display for Space {
    /// `voyage/voyage-3.5 (1024 dims)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{} ({} dims)",
            self.provider, self.model, self.dimensions
        )
    }
}

/// The configured embedder writes into a different space than the stored
/// vectors belong to. Nothing mixes them: the vector legs go dark and
/// `ingest embed` refuses; `ingest reembed --yes` switches the database.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub struct SpaceMismatch {
    /// What the embedder produces.
    pub configured: Space,
    /// What the database holds (`embedding_space`).
    pub stored: Space,
}

impl fmt::Display for SpaceMismatch {
    /// Names both spaces and the ways out. When only the model name differs
    /// the cheap way out comes first: the row may simply be mislabelled (the
    /// migration that introduced it guessed `voyage-3.5` for pre-existing
    /// vectors), and a one-line `UPDATE` costs nothing where a re-embed pays
    /// for every row.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { configured, stored } = self;
        write!(
            f,
            "embedding space mismatch: configured {configured}, database holds {stored}; "
        )?;
        if configured.provider == stored.provider && configured.dimensions == stored.dimensions {
            write!(
                f,
                "if the stored vectors were in fact produced by {}, relabel them: UPDATE embedding_space SET model = '{}'; otherwise ",
                configured.model, configured.model
            )?;
        }
        write!(
            f,
            "run `ingest reembed --yes` to switch (re-embeds everything), or configure the stored model"
        )
    }
}

/// An [`Embedder`] that knows which space its vectors belong to. Every
/// embedder in this crate implements it; the adapters that write or query
/// vector columns take this, not a bare `Embedder`, so an embedder of
/// unknown space cannot reach a column.
pub trait WithSpace: Embedder {
    /// The space `embed` writes into. `space().dimensions` equals `dimensions()`.
    fn space(&self) -> &Space;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(provider: Provider, model: &str, dimensions: usize) -> Space {
        Space {
            provider,
            model: model.to_owned(),
            dimensions,
        }
    }

    #[test]
    fn same_space_passes_and_any_difference_fails() {
        let voyage = space(Provider::Voyage, "voyage-3.5", 1024);
        assert_eq!(voyage.check(&voyage.clone()), Ok(()));
        for other in [
            space(Provider::OpenAi, "voyage-3.5", 1024),
            space(Provider::Voyage, "voyage-3-large", 1024),
            space(Provider::Voyage, "voyage-3.5", 512),
        ] {
            let err = voyage.check(&other);
            assert_eq!(
                err,
                Err(SpaceMismatch {
                    configured: voyage.clone(),
                    stored: other.clone()
                })
            );
            let msg = err.map_or_else(|e| e.to_string(), |()| String::new());
            assert!(
                msg.contains("voyage/voyage-3.5 (1024 dims)")
                    && msg.contains(&other.to_string())
                    && msg.contains("reembed"),
                "{msg}"
            );
            // Only a model-name difference suggests the relabel, which is free.
            let relabel = msg.contains("UPDATE embedding_space SET model = 'voyage-3.5'");
            assert_eq!(relabel, other.model == "voyage-3-large", "{msg}");
        }
    }

    #[test]
    fn provider_round_trips_through_its_stored_form() -> Result<(), UnknownProvider> {
        for p in [Provider::Voyage, Provider::OpenAi] {
            assert_eq!(p.as_str().parse::<Provider>()?, p);
            assert_eq!(p.to_string(), p.as_str());
        }
        assert_eq!(
            "cohere".parse::<Provider>(),
            Err(UnknownProvider("cohere".into()))
        );
        Ok(())
    }
}
