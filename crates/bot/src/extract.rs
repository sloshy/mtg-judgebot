//! `Extractor` adapter: one low-effort Anthropic call with the `Extraction`
//! schema as structured output (pipeline steps 1 + 3).

use async_trait::async_trait;
use judge_anthropic::Client;
use judge_core::{Extraction, Extractor, JudgeError, Qa, Question};

pub struct AnthropicExtractor {
    pub client: Client,
}

#[async_trait]
impl Extractor for AnthropicExtractor {
    async fn extract(&self, _q: &Question, _history: &[Qa]) -> Result<Extraction, JudgeError> {
        let _ = &self.client;
        todo!("messages request with effort=low, output_config.format = anthropic_schema::<Extraction>()")
    }
}
