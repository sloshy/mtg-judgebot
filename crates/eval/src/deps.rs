//! Build the `judge()` dependencies for the `answer` subcommand.
//!
//! Uses the bot's real composition (`judge_bot::build_deps_with`: Postgres
//! resolver/retriever, the optional embedder behind its space check, the model-driven extractor
//! and synthesizer with the production prompt and budget). With `gold_extraction`
//! the extractor is replaced by one driven by the gold file (cards, categories
//! and source keyed by question id), which isolates retrieval + synthesis
//! quality from the extractor and saves one LLM call per question.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use judge_bot::{DepsConfig, Models, db::Vectors};
use judge_core::{Deps, Extraction, Extractor, JudgeError, Qa, Question};
use sqlx::PgPool;

use crate::gold::Gold;

/// Wire the pipeline; see the module docs for `gold_extraction`.
pub fn build(
    pool: PgPool,
    models: &Models,
    vectors: Option<Arc<Vectors>>,
    cfg: &DepsConfig,
    gold: &Gold,
    gold_extraction: bool,
) -> Deps {
    let mut deps = judge_bot::build_deps_with(pool, models, vectors, cfg);
    if gold_extraction {
        deps.extractor = Arc::new(GoldExtractor::new(gold));
    }
    deps
}

struct GoldExtractor {
    by_id: HashMap<String, Extraction>,
}

impl GoldExtractor {
    fn new(gold: &Gold) -> Self {
        let by_id = gold
            .questions
            .iter()
            .map(|q| (q.id.clone(), q.extraction()))
            .collect();
        Self { by_id }
    }
}

#[async_trait]
impl Extractor for GoldExtractor {
    async fn extract(&self, q: &Question, _history: &[Qa]) -> Result<Extraction, JudgeError> {
        self.by_id
            .get(&q.thread_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no gold entry for question {}", q.thread_id).into())
    }
}
