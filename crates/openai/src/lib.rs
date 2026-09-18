//! OpenAI-compatible chat completions (`POST {base_url}/chat/completions`)
//! as a `judge-llm` backend: `OpenAI` itself, `LiteLLM`, `OpenRouter`, Ollama,
//! vLLM, llama.cpp, Azure `OpenAI`, and the OpenAI-compatible endpoints of
//! Bedrock and Vertex. Hand-written like the Anthropic client: we own the
//! types, there is no SDK.
//!
//! * [`wire`]    — serde types for the request and the response.
//! * [`schema`]  — schemars → `OpenAI` strict-mode schema (every object
//!   closed, every property required).
//! * [`convert`] — neutral [`judge_llm::ChatRequest`] ↔ chat completions,
//!   under a [`Dialect`].
//! * [`client`]  — [`OpenAi`], the [`judge_llm::Backend`] (metered by
//!   `judge-llm` before the pipeline sees it), and [`Auth`].
//!
//! "OpenAI-compatible" servers differ in small, known ways, and [`Dialect`]
//! is the explicit, closed list of those: how structured output is asked
//! for, whether tools may be strict, whether `reasoning_effort` is accepted,
//! which name the output ceiling goes by, and whether `cache_control` hints
//! are forwarded (`LiteLLM` does, for Anthropic upstreams). Everything else
//! follows `OpenAI`'s documented API; a server that needs more than these
//! knobs is a different backend, not a knob. Not modelled: the Responses
//! API.

pub mod client;
pub mod convert;
pub mod schema;
pub mod wire;

pub use client::{Auth, OpenAi};
pub use convert::BACKEND;
pub use judge_llm::ApiKey;
pub use schema::{OpenAiStrict, to_openai_strict};

/// How an OpenAI-compatible server departs from `OpenAI`. The defaults are
/// right for `OpenAI` and `LiteLLM`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dialect {
    /// How the output schema is asked for.
    pub structured_output: StructuredOutputMode,
    /// Send `strict: true` on tools (the server guarantees the arguments
    /// validate). Off for servers that reject the field.
    pub strict_tools: bool,
    /// Send `reasoning_effort` (`OpenAI` reasoning models). Off for servers
    /// and models that reject the field.
    pub reasoning_effort: bool,
    /// The name of the output ceiling parameter.
    pub max_tokens_param: MaxTokensParam,
    /// Forward `cache_control` on system and user blocks, as `LiteLLM` honours
    /// for Anthropic upstreams. Off for servers that reject content parts
    /// with unknown keys.
    pub cache_hints: bool,
}

impl Default for Dialect {
    fn default() -> Self {
        Self {
            structured_output: StructuredOutputMode::JsonSchema,
            strict_tools: true,
            reasoning_effort: false,
            max_tokens_param: MaxTokensParam::MaxTokens,
            cache_hints: false,
        }
    }
}

/// How a server can be asked for the output schema.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StructuredOutputMode {
    /// `response_format: {type: json_schema, json_schema: {schema, strict: true}}`;
    /// the output is constrained to the schema.
    #[default]
    JsonSchema,
    /// `response_format: {type: json_object}`: valid JSON of no particular
    /// shape; the schema goes into the prompt.
    JsonObject,
    /// No `response_format`; the schema goes into the prompt.
    Prompt,
}

/// Which name the output ceiling is sent under.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MaxTokensParam {
    /// `max_tokens`: most compatible servers.
    #[default]
    MaxTokens,
    /// `max_completion_tokens`: `OpenAI`, which rejects `max_tokens` on reasoning models.
    MaxCompletionTokens,
}

impl Dialect {
    /// What a server with this dialect can enforce, reported honestly from
    /// the knobs: the pipeline reads it to decide whether the schema must go
    /// into the prompt and to log what it relies on. Refusal fallbacks are
    /// an Anthropic feature and never exist here.
    #[must_use]
    pub fn capabilities(self) -> judge_llm::Capabilities {
        judge_llm::Capabilities {
            structured_output: match self.structured_output {
                StructuredOutputMode::JsonSchema => judge_llm::StructuredOutput::Enforced,
                StructuredOutputMode::JsonObject => judge_llm::StructuredOutput::JsonMode,
                StructuredOutputMode::Prompt => judge_llm::StructuredOutput::PromptOnly,
            },
            strict_tools: self.strict_tools,
            effort: self.reasoning_effort,
            cache_hints: self.cache_hints,
            refusal_fallbacks: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use judge_llm::StructuredOutput;

    #[test]
    fn capabilities_follow_the_knobs() {
        let d = Dialect::default();
        let c = d.capabilities();
        assert_eq!(c.structured_output, StructuredOutput::Enforced);
        assert!(c.strict_tools && !c.effort && !c.cache_hints && !c.refusal_fallbacks);
        let d = Dialect {
            structured_output: StructuredOutputMode::JsonObject,
            strict_tools: false,
            reasoning_effort: true,
            cache_hints: true,
            ..d
        };
        let c = d.capabilities();
        assert_eq!(c.structured_output, StructuredOutput::JsonMode);
        assert!(!c.strict_tools && c.effort && c.cache_hints && !c.refusal_fallbacks);
        assert_eq!(
            Dialect {
                structured_output: StructuredOutputMode::Prompt,
                ..d
            }
            .capabilities()
            .structured_output,
            StructuredOutput::PromptOnly
        );
    }
}
