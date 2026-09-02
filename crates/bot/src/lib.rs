//! `judge-bot` as a library: the sqlx adapters for the DB-backed ports, the
//! model adapters (extraction and synthesis over any `judge_llm::ChatModel`),
//! the [`config`] loader that picks providers and models, and
//! [`build_deps`], the one composition shared by the `bot`, `api`, `eval`
//! and `agent` binaries.

pub mod config;
pub mod db;
pub mod discord;
pub mod extract;
pub mod session;
pub mod synth;

use std::sync::Arc;

use judge_core::{Deps, Retriever};
use judge_llm::{Backend, ChatModel, LlmError, Metered, Price, SpendMeter, SynthConfig};
use sqlx::PgPool;

use db::{PgResolver, PgRetriever, Vectors};

/// The models the pipeline runs on: one per stage, both billed to one
/// meter. The stages may share one model (the zero-config setup) or not;
/// the meter is one per process either way, so a front door reads the whole
/// spend. The fields are private and the constructors wrap the backends
/// themselves, so a `Models` cannot hold an uncapped model or report a meter
/// its models do not bill to.
#[derive(Clone)]
pub struct Models {
    extract: Arc<dyn ChatModel>,
    synth: Arc<dyn ChatModel>,
    meter: SpendMeter,
}

impl std::fmt::Debug for Models {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Models")
            .field("extract", &format_args!("{}/{}", self.extract.provider(), self.extract.model()))
            .field("synth", &format_args!("{}/{}", self.synth.provider(), self.synth.model()))
            .field("meter", &self.meter)
            .finish()
    }
}

impl Models {
    /// One model for both stages, behind `meter` at the built-in table's price.
    ///
    /// # Errors
    /// `Unpriced` when the table cannot price the model.
    pub fn single<B: Backend + 'static>(meter: SpendMeter, model: B) -> Result<Self, LlmError> {
        let model: Arc<dyn ChatModel> = Arc::new(Metered::new(model, meter.clone())?);
        Ok(Self { extract: Arc::clone(&model), synth: model, meter })
    }

    /// One model per stage, both behind `meter` at the built-in table's price.
    ///
    /// # Errors
    /// `Unpriced` when the table cannot price either model.
    pub fn pair<A: Backend + 'static, B: Backend + 'static>(meter: SpendMeter, extract: A, synth: B) -> Result<Self, LlmError> {
        Ok(Self {
            extract: Arc::new(Metered::new(extract, meter.clone())?),
            synth: Arc::new(Metered::new(synth, meter.clone())?),
            meter,
        })
    }

    /// One model per stage at explicit prices, both behind `meter`: what
    /// [`config::Config::models`] builds, where the price came from the file
    /// or the provider is free.
    #[must_use]
    pub fn priced<A: Backend + 'static, B: Backend + 'static>(
        meter: SpendMeter,
        extract: A,
        extract_price: Price,
        synth: B,
        synth_price: Price,
    ) -> Self {
        Self {
            extract: Arc::new(Metered::priced(extract, meter.clone(), extract_price)),
            synth: Arc::new(Metered::priced(synth, meter.clone(), synth_price)),
            meter,
        }
    }

    /// The extraction/classification model (cheap, low effort).
    #[must_use]
    pub fn extract(&self) -> Arc<dyn ChatModel> {
        Arc::clone(&self.extract)
    }

    /// The synthesis model (the one that answers).
    #[must_use]
    pub fn synth(&self) -> Arc<dyn ChatModel> {
        Arc::clone(&self.synth)
    }

    /// The spend counters both models bill to.
    #[must_use]
    pub fn meter(&self) -> &SpendMeter {
        &self.meter
    }

    /// The zero-configuration setup, from the environment: Anthropic's
    /// first-party API with `ANTHROPIC_API_KEY` (and `ANTHROPIC_BASE_URL`),
    /// `claude-opus-5` for both stages, one spend cap from `JUDGE_MAX_USD`.
    /// The binaries go through [`config::Config::load`], which builds this
    /// same setup when there is no `judge.toml`.
    ///
    /// # Errors
    /// `MissingApiKey`, `BadMaxSpend`, or if the HTTP client cannot be built.
    pub fn from_env() -> Result<Self, LlmError> {
        Self::single(SpendMeter::from_env()?, judge_anthropic::Anthropic::from_env()?)
    }
}

/// Knobs for [`build_deps_with`]; [`Default`] is what [`build_deps`] uses.
#[derive(Clone, Debug, Default)]
pub struct DepsConfig {
    /// Extraction request knobs: effort, output ceiling, history turns.
    pub extract: extract::ExtractConfig,
    /// Synthesis request knobs: effort, output ceiling, refusal fallback.
    pub synth: SynthConfig,
    /// Size caps for the synthesizer's user turn.
    pub budget: synth::Budget,
}

/// Wire the Postgres adapters and the model adapters into [`Deps`] with
/// [`DepsConfig::default`]. `vectors` is optional: without one the retriever
/// skips its vector leg and orders prior calls by recency. A binary that also
/// builds a `PgCallStore` hands it the same `Arc` ([`config::Config::vectors`]),
/// so the space check runs once.
#[must_use]
pub fn build_deps(pool: PgPool, models: &Models, vectors: Option<Arc<Vectors>>) -> Deps {
    build_deps_with(pool, models, vectors, &DepsConfig::default())
}

/// [`build_deps`] with explicit configuration.
#[must_use]
pub fn build_deps_with(pool: PgPool, models: &Models, vectors: Option<Arc<Vectors>>, cfg: &DepsConfig) -> Deps {
    let mut retriever = PgRetriever::new(pool.clone());
    if let Some(v) = vectors {
        retriever = retriever.with_vectors(v);
    }
    let retriever: Arc<dyn Retriever> = Arc::new(retriever);
    let synthesizer =
        synth::LlmSynthesizer::new(models.synth(), cfg.synth.clone(), Arc::clone(&retriever)).with_budget(cfg.budget);
    Deps {
        extractor: Arc::new(extract::LlmExtractor::new(models.extract(), cfg.extract.clone())),
        resolver: Arc::new(PgResolver::new(pool)),
        retriever,
        synthesizer: Arc::new(synthesizer),
    }
}
